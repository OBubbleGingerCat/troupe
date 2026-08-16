# Troupe Production Diagnostics 设计

- 状态：Draft
- 设计版本：V0
- 实现状态：未开始
- 最后更新：2026-08-14
- 相关设计：[Actor.act() ACP Agent Session](./actor-agent-session.md)
- 交互基线：本地 draft `docs/draft/diagnostics-ui-demo/`（不属于正式规范）

## 0. 文档范围

本文记录 Troupe diagnostics 的产品边界和架构方向。当前已经确定的是：Troupe 自己提供
随 Production 实时更新的诊断服务与 Web UI，Perfetto 只作为 CLI 离线导出格式；事件的 V1
逻辑模型、SSE/JSON wire protocol、持久化 engine/durability/retention、diagnostic CLI、
trusted-LAN/no-auth 网络边界、Python 扩展与视图、Web UI 发布和 Perfetto exporter 均已由 D1-D54
冻结；当前没有待决的编号设计项。

本文讨论的 diagnostics 是运行中 Production 的结构化可观测性，不是当前
`rust/src/application/diagnostics.rs` 中面向 stderr 的异常 traceback formatter。后者保留现有
用户可见错误输出职责；新体系不能取代 public Production exception contract，也不能让诊断数据参与
业务 correctness 判定。diagnostic core 自身是 Runtime 的强制基础设施，其失效终止 Runtime 的规则由
D16 单独定义。

## 1. 目标与非目标

### 1.1 目标

- Runtime Run 启动一个可从本机或其他机器访问的 diagnostic server。
- 在 Production package 的 `.troupe` 目录中发布一个简单、可校验的 active-instance registry，
  供本机 `troupe diagnostic` 自动发现服务。
- Web UI 在 Production 运行期间增量更新，展示 Production、Scene、Actor、Cue、Effect、
  `Actor.act()`、agent turn、tool、result validation、usage、错误与取消的时间和因果关系。
- Web UI 和 CLI 查询同一个版本化事件事实源；UI 不维护一套只供展示使用的第二份语义。
- 恢复先前延期的 `Actor.act()` diagnostics，但不改变 `act()` 的业务返回值和异常契约。
- `Actor.act()` 接受可选的、可继承的 Python `DiagnosticSink`，让当前 Act 的调用方实时消费
  normalized agent message、plan、tool、result validation、context usage 和可用 token usage。
- 每个已开始 Act 都产生明确的终态 token accounting；agent/provider 能可靠报告时展示本 Act 的
  token 总量与分类，否则明确标记 unavailable，不能把缺失解释为零。
- Python 可以发布领域诊断事件，并声明 Troupe Web UI 能渲染的自定义视图。
- `troupe diagnostic dump` 可以从一个一致的事件水位导出标准 Perfetto `.pftrace`，供用户在
  public Perfetto 中自行打开。
- diagnostics 对超长期 Production 可用，并明确处理重连、慢客户端、留存和敏感数据。
- 完整 Production Runtime 只在 diagnostic server、canonical event pipeline 和持久化存储持续健康时
  运行；基础设施失效不能静默产生一个不可观测的 agent flow。

### 1.2 非目标

- 不 vendor、fork、构建、发布或嵌入 Perfetto Web UI。
- 不要求最终用户安装 npm、Node.js 或 Perfetto 源码。
- 不把 Perfetto TrackEvent 或 Trace Processor schema 当成 Troupe 的内部事实模型。
- 不通过 diagnostics API 控制、暂停、取消或修改 Production；第一版控制面只读。
- 不把 diagnostics 当成 correctness journal、事务日志、确定性 replay 或审计日志。
- 不在 producer/agent hot path 上同步调用任意 Python callback，也不执行 Python 生成的浏览器
  JavaScript。
- 不把 prompt、agent thought/reasoning、tool 参数/输出、文件内容、环境变量或 validated result
  默认写入诊断数据。面向用户的 agent message 是实时诊断的正式内容，不属于 reasoning。
- 不允许 `DiagnosticSink` 通过 callback 返回值、异常或延迟改变当前 `act()` 的 result acceptance、
  settlement、cancellation 或 session health。

## 2. 已确认的设计决策

| ID | 决策 |
|---|---|
| D1 | Production 与 `Actor.act()` diagnostics 共用一份 Run 级、版本化的 Troupe diagnostic event stream。Run hub 与 per-Act Python sink 是同一 normalized producer 的不同订阅者，不各自解释 provider update。 |
| D2 | Diagnostic server 由 Runtime Run 所有。Active Run 的 Web UI 与 `troupe diagnostic` CLI 都是这个服务的只读客户端；completed local archive 通过 D28 的同一只读 server/query implementation 访问，不在 CLI 中另造一套事件解释逻辑。 |
| D3 | Runtime 默认监听 `0.0.0.0` 的 OS-assigned port，并在 Production package 的 `.troupe` 下发布 active-instance registry，以支持本机发现和远端访问。advertised URL 由 D22 定义；并发实例格式由 D19-D20 定义。 |
| D4 | 实时页面由 Troupe 自己实现并直接消费增量事件；页面必须在更新时保留展开、选择、缩放、过滤和 follow-now 状态。 |
| D5 | Perfetto 不出现在实时页面中。它只是一种按需导出格式；用户将 `.pftrace` 文件自行加载到 public Perfetto。 |
| D6 | Troupe 只携带生成 `.pftrace` 所需的最小编码实现，不携带 Perfetto UI、Trace Processor 或其前端构建链。 |
| D7 | `Actor.act()` 成功时仍然只返回 schema-validated Python `dict`。usage、trace、effective model/effort、provider/session identity、operation identity 和 timing 只能进入独立 diagnostic 通道，不能进入 wrapper、tuple 或 reserved result key。 |
| D8 | 同一 Actor 在同一 Scene 内的多个 Cue 必须作为独立、可折叠的 Cue 分组展示；每个 Cue 分别展示 mailbox wait、`cued()`、Act、tool、Effect 和 outcome。 |
| D9 | Python 扩展采用“per-Act `DiagnosticSink` + 结构化业务事件 + 声明式视图”三层模型；sink 消费 canonical event，业务插桩发布 custom variant，视图只声明由 Troupe Web UI 执行的受限查询和内置 renderer。具体 public contract 由 D34-D38 与 D41-D44 冻结。 |
| D10 | `Actor.act()` 增加 keyword-only `diagnostic_sink: DiagnosticSink | None = None`。sink 是 observer，不进入 validated result 或 public agent exception contract。 |
| D11 | 面向用户的 agent message 需要在 Web UI 和 per-Act sink 中实时可见；agent thought/reasoning 不采集、不展示，也不能混入 agent message。 |
| D12 | Context occupancy 与 per-Act token accounting 是独立指标。后者统计一次 Act 中整个 agent turn 的所有 model request，只接受 agent/provider 直接报告且可归属当前 Act 的终态 accounting；不通过 context 差值、本地 tokenizer 或 session 累计 counter 推算。 |
| D13 | `DiagnosticSink.on_event()` 与 Web UI 的事实流共用同一个 canonical `DiagnosticEvent` typed hierarchy 和语义。per-Act sink 只按当前 `act_id`、capture 与 resource policy 接收投影，不定义或包装第二套 Act event；snapshot、连接状态、订阅者 delivery gap、resync 等是 live transport control message，不属于 `DiagnosticEvent`，也不发送给 sink。 |
| D14 | `DiagnosticEvent` 是 14 个 immutable variant 的 closed discriminated union，所有 variant 共用 Run identity、全局 sequence、monotonic elapsed time、domain scope 和 typed backward causal links。Span 使用 append-only start/finish pair，start event 的 sequence 同时是 span identity；不另设 `event_id` 或 Act-local sequence。开发阶段 public Python type name 不携带 schema version suffix；wire/store 中的独立 `schema_version` 仍用于兼容性校验。 |
| D15 | V1 默认记录结构、生命周期、user-visible agent message、plan、tool metadata、result validation metadata、usage、稳定错误码和有界 custom attributes；默认排除 script/prompt、thought/reasoning、tool input/output、文件内容、validated result value、provider raw payload 和独立认证字段。显式 opt-in 的 tool payload 是受类型与大小限制的 opaque content；Troupe 不检查、识别、按 key 脱敏或改写其内容。Canonical `ObservationGap` 与 subscriber-local delivery loss 必须分开。 |
| D16 | 完整 Production Runtime 强制启用 diagnostic server 和持久化事件存储，不提供 `off`、best-effort 或无持久化模式。server、canonical event pipeline、durable writer、active reader/store invariant 或 active query execution context 无法启动或在运行中被 supervisor 判定失效时，Runtime 必须停止 Production 并以非零状态结束。单个客户端、archive reader/query、可选 Python sink 或按需 exporter 故障不属于 core failure。 |
| D17 | 状态根固定为 `<production-root>/.troupe/`。Runtime 必须在执行任何 Production 用户代码前创建并通过真实写入验证该目录；失败即启动失败。不支持 registry-root override，也不静默回退到临时目录、用户目录或其他位置。 |
| D18 | V1 diagnostic server 是 Runtime OS 进程内由 Runtime 监督的组件，不是独立 daemon 或子进程。逻辑配置默认 `bind_host=0.0.0.0`、`port=0`，允许通过 D33 的 Runtime CLI flags 显式覆盖；store、listener 和 registry 全部 ready 后才允许 import/构造 Production。 |
| D19 | Registry 使用 `.troupe/diagnostics/instances/<run-id>.json` 的 per-Run immutable locator；持久化 Run 数据属于独立的 `.troupe/diagnostics/runs/<run-id>/` namespace。并发 Run 不互相覆盖，不提供 singleton `active.json` 或隐式 `latest` pointer；多个 active Run 存在时，调用方必须显式选择。 |
| D20 | Instance entry 只在 store/listener ready 后通过 same-directory temporary file 原子发布，并在 server 真正停止前先撤销发布；它不保存动态 Production 状态。客户端必须同时校验 owner process identity 与 server `run_id`，只自动删除能够证明 owner 已消失或 PID 已复用的 definite-stale entry；unreachable、identity mismatch、损坏或 newer-schema entry 按各自状态报告并保守保留。 |
| D21 | V1 diagnostic server 不实现认证、授权、session 或 credential，部署边界明确限定为 trusted LAN；任何能连接 listener 的网络 peer 都能读取全部已采集诊断数据。UI、API 和 live stream 同源，server 不通过 CORS header 授权跨源浏览器读取；所有 endpoint 保持只读。 |
| D22 | V1 直接服务使用 plain HTTP，不内置 TLS。可选显式 `advertise_url` 只用于 registry/identity 中的远端发现和展示，不改变 bind，也不由 Troupe 猜测 host IP/DNS；未配置时只发布从 bind 派生的本机可连接 endpoint（wildcard bind 使用 loopback），远端客户端显式提供 URL。跨不可信网络必须使用外部 VPN、SSH tunnel 或 TLS-terminating reverse proxy。 |
| D23 | V1 live transport 使用同源 SSE + UTF-8 JSON，不提供 WebSocket。客户端先取得 committed snapshot watermark `W`，再以 finite exact range 取得 `(max(0,W-4096),W]` 的有界 canonical event suffix并与snapshot原子hydrate，最后从持久化 store replay `W` 之后的 event并进入live committed tail；delivery 是 at-least-once，单连接内 canonical sequence 严格递增，客户端按 `(run_id, sequence)` 去重。 |
| D24 | SSE 每个 `diagnostic_event` frame 只携带一个 canonical event，并以十进制 sequence 作为 `id`。`stream_ready/heartbeat/delivery_gap/resync_required/stream_closed` 是无 `id`、不推进 cursor 的 transport control。所有 schema-declared `u64` wire value 使用 canonical decimal string；慢客户端 buffer overflow 时尽力发送 `delivery_gap` 后断开，不能静默跳过事件并继续。 |
| D25 | 每个 Run 在 `.troupe/diagnostics/runs/<run-id>/diagnostics.sqlite3` 独占一个 SQLite database，使用 WAL、`synchronous=FULL` 和单一有序 writer；不同 Run 不共享 database、writer 或 WAL。Canonical event、committed watermark、Run metadata 和可重建 materialized read model 在同一 transaction 中提交。 |
| D26 | Store 中可见的 event 始终构成 sequence `1..W` 的 dense committed prefix；snapshot、replay、live SSE 和 exporter 只能读取该 prefix。Writer 使用有界 group commit，只有 SQLite `COMMIT` 在 `FULL` durability boundary 成功后才推进 `W` 并发布 live notification。异常进程/机器终止可以丢失 accepted-but-uncommitted tail，但不得丢失已成功 commit 的 transaction（以 SQLite、OS、filesystem/hardware 实际提供的 durability guarantee 为边界）。 |
| D27 | Mandatory writer ingress 的 accepted-but-uncommitted budget 默认最多容纳 32,768 events 或 64 MiB canonical encoded bytes，包含 queued 与 in-flight transaction，先达到者为限；默认 batch trigger 为 oldest event 25 ms、512 events 或 1 MiB。Budget exhaustion、writer/commit failure 或越过有限 progress/drain deadline 都是 fatal core failure，不允许丢 event 后继续。Active Run 不裁剪历史；可选 `max_run_bytes` 达限同样终止 Production。Completed archive 默认无限期保留，显式 cleanup 只能删除整个 inactive、unleased Run directory。 |
| D28 | Diagnostic server 不在 Production 结束后 daemonize。正常结束先停止新工作、持久化 terminal facts 和最终 metadata、完成 bounded drain，再发送 `stream_closed`、durably unpublish registry、关闭 listener/store；持久化失败使 archive 保持 incomplete 且进程非零。Run directory 继续作为 archive；本地 archive 由 CLI 通过同一只读 query implementation 临时提供，浏览器使用 D32 的显式 foreground、loopback-only archive server。 |
| D29 | V1 新增真正的顶层 `troupe diagnostic` command branch，同时保留现有 `troupe --production <package> -- <production-args>` run syntax。Diagnostic query command 的 target 必须且只能是 `--production <package> [--run <uuid>]`、`--url <base-url>` 或 `--archive <run-directory>`；`--archive` 指完整 Run directory，不接受裸 SQLite file。所有 diagnostic command 都不得 import/构造 Production。 |
| D30 | `--production` local resolver 不维护或猜测 `latest`。显式 `--run` 连接 identity-validated active server，否则读取无 live owner 的同 ID archive；没有 `--run` 时，唯一可安全确认的 active Run 优先于历史 archive，多个/ambiguous live instance 要求显式选择；没有 live instance 时只有唯一 archive 才可隐式选择。`unhealthy`、`identity_mismatch`、`invalid` 或 `incompatible` instance 不能通过直接读 SQLite 绕过，只有 revalidated `definite_stale` 可以清理 entry 后回退 archive。 |
| D31 | V1 command set 为 `runs/status/snapshot/events/dump/serve/cleanup`。Finite query 默认 human output 并提供 versioned JSON；`events` 提供 human 或 canonical JSONL，stdout 一行一个 event，绝不混入 SSE control。`events` 默认 `--tail 100`，与 `--after` 互斥，archive 禁止 `--follow`。`dump` 只生成 Perfetto：local/archive target直接读取captured prefix，URL/active target调用同一server的只读dump endpoint，CLI均在调用方本机atomic publish且默认不覆盖。成功读取 failed/incomplete Run 仍 exit 0；operation/protocol/store failure 为 1，usage error 为 2，用户中断为 130。 |
| D32 | `diagnostic serve` 只接受 inactive `--production ... --run ...` 或 `--archive ...`，以前台 loopback-only、OS-assigned port、archive lease 和可选 `--open` 提供同一 Web/query implementation；不发布 active registry。`diagnostic cleanup` 只操作 `--production` 下经验证的 whole Run directory，默认 preview、`--apply` 才删除，且每次只接受 exact Run、age、keep-count 或 total-byte policy 之一；active/leased Run 永不删除，batch policy 不自动删除 incomplete archive。 |
| D33 | Production run 的 V1 diagnostic flags 固定为 `--diagnostic-bind-host`、`--diagnostic-port`、`--diagnostic-advertise-url`、`--diagnostic-max-run-bytes`、`--diagnostic-writer-stall-timeout` 和 `--diagnostic-shutdown-timeout`，全部位于 Production `--` separator 之前；stall/drain 默认分别为 10 s/30 s。V1 不暴露 disable、alternate-root、auth、queue、batch 或 automatic-retention flag。Registry ready 后 Runtime 在 stderr 输出一行 versioned JSON locator，stdout 保持属于 Production。 |
| D34 | `DiagnosticCapture` 是 bind-time frozen、keyword-only public value；agent message/thinking activity、plan、context/terminal usage、tool metadata、result-validation metadata 和 custom event 默认开启，tool input/output 默认关闭。八个 strict-bool field 按第 4.5 节的 closed event-kind matrix 过滤；`usage=False` 必须同时关闭 `ContextUsageSampled` 和 `ActTokenUsageFinalized`，两者仍是语义不同的事件；Act/caller/turn lifecycle、相关 gap，以及仅属于当前 Act 的 `agent.turn.active` 与累计 `diagnostic.dropped_events` counter不可关闭，mailbox/Cue/Run级counter不进入Act sink。普通 subscriber drop 只更新该 counter；sink enqueue/callback channel 首次故障形成 Run-store/Web/CLI 可见的 typed `diagnostic.component_failed`，但任何 sink-targeted component failure 均不再投递给 per-Act sink。`DiagnosticSink` 必须初始化、只绑定一个成功 admitted Act 一次，公共状态误用由 `DiagnosticSinkStateError(code=uninitialized/unbound/already_bound)` 表达；`wait_closed()` 可重复等待同一 immutable summary，不提供 timeout 参数或 public force-close。 |
| D35 | `DiagnosticSinkSummary` 冻结 Act/delivery outcome、成功交付范围、drop/source-gap/truncation、callback failure 与 abandoned 状态；`complete` 只表示所请求 capture 的证据是否完整，不等于 Act 成功。Callback 抛错、自发取消或返回/await 出非 `None` 只终止该 sink，并形成有界 `DiagnosticCallbackFailure`，绝不从 `act()` 或 `wait_closed()` 重抛，也不改变 agent/session。Summary 不重复 token accounting 字段或 usage event pointer；sink 需要时保留收到的 canonical terminal event。 |
| D36 | 每个 Runtime 使用一个独立 daemon thread 上的 diagnostic asyncio loop 执行 Python sink；同一 sink 严格串行，不同 sink 作为独立 task仅在callback yield时交错运行，且 callback 不继承 Cue/Actor authority。阻塞型同步callback会占住这个diagnostic loop并延迟其他sink，但不阻塞mandatory hub或Production；shutdown deadline可以abandon它。每 sink delivery 固定上限为 1,024 events/8 MiB，并在其中保留 32 events/256 KiB 给结构与 terminal；Runtime 全部 sink 合计固定为 16,384 events/64 MiB。Overflow 只造成有计数的 subscriber-local loss 或关闭该 sink，不反压 agent、不伪造 `ObservationGap`、不终止 Production。 |
| D37 | Agent text chunk 只在 normalization、canonical sequence 分配前按相同 normalized message ID 合并，并在 16 KiB、20 ms、同 Act 其他 canonical event 或 turn terminal 时 flush；sequence 分配后任何消费者都不能再合并或改写。缺失 provider message ID 的 chunk 在一个 Act 内共用一个 anonymous synthetic message，不按文本、时间、tool/plan/usage/reasoning interleave 猜边界；显式 ID change 结束上一显式消息，turn terminal 按首次出现顺序结束所有 open message。 |
| D38 | 显式 tool input/output capture 只投影给该 sink，包含 ACP stable raw input/output、content 和 locations，排除 protocol envelope 与 `_meta`；payload 在来源选择后视为 opaque，Troupe 只执行类型/大小校验。P00 定义 immutable public `FrozenJsonArray`、`FrozenJsonObject`、closed `FrozenJsonValue` alias、`DiagnosticToolInput`、`DiagnosticToolOutput`和`DiagnosticToolLocation`；tool start/update detail 的 `captured_input`/`captured_output` 仅在相应 opt-in sink projection 中非 `None`，canonical store/Web/Perfetto projection 始终为 `None`，因此事件 hierarchy 与类型不分叉。Typed payload 最大 depth 32、nodes 65,536，每个 input/output snapshot 最大 256 KiB、每 Act tool payload 合计 4 MiB；agent message 最大每条 4 MiB/每 Act 16 MiB，plan snapshot 最大 256 KiB。超限原子省略字段或停止后续文本并显式标记 truncation，不生成非法 partial JSON。 |
| D39 | `ActTokenUsageFinalized` 是 immutable、slotted、keyword-only canonical event，精确携带 `availability`、实际 `source`、仅 unavailable 时存在的 closed reason，以及 provider total/input/output/thought/cache 六个 optional token 字段。Public token value 的产品语义是排除 `bool` 的非负 Python `int`，不声明 `u64` 或其他 Troupe product maximum；零是 observation，`None` 是 unknown，分类之间不建立加和约束。 |
| D40 | 每个 started Act 在 accounting 终态已知后、`act.lifecycle` finish 前恰好产生一个 usage event。唯一 finalization transition 必须在三类可证明边界之一线性化：prompt 未提交的 Act terminal、已提交但无 settlement 的 session terminal、或 authoritative turn settlement；第一类为 `prompt_not_submitted`，第二类为 `turn_settlement_unknown`，第三类再按 source qualification/report presence 决定。V1 只接受 whole-turn 验收通过的 `acp.prompt_response.usage` carrier。各 token 字段独立聚合 known sum 与 reported/finalized coverage，并统计 availability；`DiagnosticSinkSummary` 不重复 accounting。Token value 在 JSON wire 中使用 canonical decimal string 只是精确编码约定，不意味着 public `u64` 上限。 |
| D41 | `troupe.diagnostics` 的 V1 publication surface 固定为同步 `event()`、绝对 gauge `counter()` 和同步 context-manager `span()`。参数立即校验、复制；`event()`/`counter()` call 与 span enter/exit 分别 admission 一个 custom event，自动继承 Runtime scope/当前 task 内 span，不允许覆盖 identity、时间、scope、parent 或 causality，也不返回 canonical identity。Act scope只来自显式generation-bound task authority，区分caller、registered caller descendant和authorized supervisor；过期authority不得回退到Cue或通过sink registry反查Act。参数或 context 错误在 sequence 分配前同步抛出；成功只表示进入 mandatory pipeline，随后 core persistence/backpressure failure 仍会终止 Production。 |
| D42 | Custom name 是至少两段的 lowercase ASCII dotted identifier，`troupe.*` 保留且 V1 不设 namespace registry。Custom value 使用受限 flat scalar/scalar-list model；name/key/unit、entry/list 数和单 event canonical bytes 受第 8.1 节固定上限约束。Counter 数值接受排除 `bool` 的 `int`、finite `float` 或 finite `Decimal`，并立即规范化为 decimal wire value；`NaN`/infinity 非法。Troupe 只校验结构和大小，不扫描、脱敏或改写业务内容。 |
| D43 | V1 `ViewSpec` 是 `TimelineView | MetricView | TableView | TimeSeriesView` 的 final、frozen、slotted、keyword-only union。每种 renderer 只接受对应的 closed typed query descriptor；允许 exact built-in kind/custom name、severity/outcome、scalar attribute equality/existence、一个 closed group dimension 和 `count/sum/min/max/mean/latest`，禁止 SQL、regex、join、任意字段路径、用户函数及自定义 renderer。每个 view 独立声明 viewport/run 时间绑定和 selection/run scope 绑定。`TimeSeriesView` 由 server 按 Run-origin、左闭右开 bucket 和固定 `max_points=1024` 生成，width 为 `max(1, ceil(duration/1023))`；browser 不重分桶，watermark/viewport/width 变化使旧结果整体 stale。 |
| D44 | `diagnostic_views` 必须是 Production class 上由 built-in ViewSpec 组成的 exact tuple。Runtime 在 Production class 解析后、constructor 前将每个 view 编译为 independently versioned pure JSON record 并持久化，之后 HTTP、live update、browser 与 archive serving 均不执行或导入 Production Python。Active Run 的无效/重复/不兼容 ViewSpec 阻止构造与启动；若 diagnostics 健康，Run 仍以 `outcome=failed, clean_shutdown=true` 完成并在执行 constructor 前释放 registry/listener/store/lease，只有 view-record commit 或 diagnostic finalization 自身失败才保持 incomplete。HTTP以manifest顺序暴露最多64项的versioned catalog：compatible项携带完整record，archive incompatible项只携带manifest identity与normalized incompatibility；newer-schema record按manifest version作为opaque data分类，不按current schema解析。archive 中不受支持或损坏的 custom view record 只局部标记 unavailable，不能阻止访问 canonical diagnostics。单次 query/renderer failure 是 client-local；active server/query/store 系统性失效仍按 core-fatal，archive 同类失败只终止该 archive operation。 |
| D45 | Production Web UI 固定使用 strict TypeScript、Preact、`@preact/signals`、Vite、tree-shaken `lucide-preact` 和 uPlot，配合手写 modular CSS/custom properties 与 system fonts。Preact DOM 负责 shell、控制、tree、inspector、transcript、table、usage 与 ViewSpec panel；framework-independent TypeScript module 负责 protocol/query state；一个 imperative Canvas2D renderer 负责层级 trace，uPlot 只负责 server-bucketed `TimeSeriesView`。V1 不引入 React compatibility、router、Redux/query framework、CSS framework/component kit、D3/ECharts、SSR 或 runtime template compilation。 |
| D46 | 浏览器只保留可见 query window、当前 detail/展开状态和有界 adjacent-window LRU；native `fetch`/`EventSource` 推进 watermark、更新 live edge 并使相关 query 失效。Bootstrap/resync把snapshot作为materialized projection唯一authority，并以最多4096项、终点精确为snapshot `W`的raw suffix只补EventTable和snapshot未表达的instant-derived tool/result事实，不能把`<=W`事件经普通live reducer重放。Pause 不保存无界 raw-event backlog，恢复时对已淘汰范围重新 query。Schema `u64` identity/cursor/time 保持 decimal string 或 `bigint`，只有相对 viewport origin 的有界 elapsed delta 可转 JavaScript `number`。V1 不使用 Web Worker、WebGL、service worker、IndexedDB、localStorage diagnostic cache、CDN、external font 或其他 external asset；Canvas 同步 tree 纵向虚拟化并保留 keyboard-operable ARIA treegrid 与 text inspector 语义面。 |
| D47 | Frontend source、exact lockfile 和 maintainer Node major 在 repository 中维护；唯一 release build 使用 pinned `npm ci`、strict checks/tests 和 deterministic Vite build，固定 relative base、ES2020、一个 JS entry、一个 CSS entry、无 dynamic chunk 和无 shipped source map。生成的 raw/Brotli/gzip assets、manifest/Rust include table 与 third-party notices checked in，CI regenerate 后要求 byte/hash equality。Rust 以 `include_bytes!` 嵌入这些 bytes；普通 maturin/sdist/wheel build 不运行 Node/npm、不访问网络，`pyproject.toml` build requirement 仍只有 maturin，wheel 不新增独立 static asset file，`.troupe` 与 Run archive 也不复制 UI asset。 |
| D48 | Embedded UI 通过 relative、content-hashed URL、exact MIME、HEAD/conditional request、representation-specific strong ETag 和预生成 Brotli/gzip negotiation 提供。HTML 使用 `no-cache` + ETag，hashed assets 使用一年 `immutable`，API/bootstrap/query 使用 `no-store`，SSE 使用 `no-cache, no-transform`，encoding negotiation 使用 `Vary: Accept-Encoding`。页面在打开 live transport 前验证 UI/API/event/ViewSpec compatibility。禁止 inline/third-party script 和 `dangerouslySetInnerHTML`；固定 CSP、`nosniff`、`no-referrer` 与 same-origin resource policy。支持下限为 Chromium/Edge 111、Firefox 115 和 Safari 16.4 及相应 mobile engine，不提供 legacy/polyfill bundle；必要能力不满足时显示静态 compatibility state。 |
| D49 | Release gate 固定为 logical uncompressed HTML+JS+CSS 不超过 512 KiB、其 first-load Brotli 总量不超过 160 KiB、全部 embedded raw/gzip/Brotli UI representations 加 notices 不超过 768 KiB。验证至少包含 strict TypeScript/unit test、Rust-browser shared canonical fixtures、Playwright Chromium/Firefox/WebKit、desktop/mobile screenshot 与 Canvas pixel check、keyboard/ARIA/axe、malformed/XSS、reconnect/gap/resync/pause、cache/CSP/compression、deterministic rebuild 和 wheel smoke。Pinned Chromium stress fixture 覆盖 long Run、10,000 visible primitives 与持续 live update，证明有界 cache/heap、无 read-model correctness loss、每 animation frame 最多一次 Canvas draw，并通过 checked-in explicit performance baseline。 |
| D50 | Perfetto exporter 只增加 exact-pinned `prost 0.14.4` runtime crate，并私有声明实际使用的 stable-public protobuf subset。Schema provenance 固定到 official Perfetto v57.2 commit `da1d152cff27890903d158fe96751de3aab883cc`，repository 保存所需 upstream proto/license、逐文件 SHA-256 和 used-field manifest；升级必须显式 review 并重建 fixtures。所有 TrackEvent/TrackDescriptor packet 使用固定 `trusted_packet_sequence_id=1` 满足 v57.2 ingestion，但不使用 incremental state/interning；counter track descriptor 携带空 `CounterDescriptor` presence marker。普通 build/runtime 不使用 `prost-build`、`prost-types`、`protoc`、第三方 exporter、Perfetto SDK/FFI、Trace Processor、Node 或网络。最小 schema 是协议可审计边界，不是包体优化。 |
| D51 | `.pftrace` 按 descriptor/metadata prelude 后接 event packet 的确定顺序流式写出，每次只编码一个 `TracePacket` 并作为 top-level `Trace.packet` field 1 写入 reusable buffer。为完成D52全局排序/lane/backward attachment，允许two-pass captured-prefix读取和一个prefix-wide structural index；V1固定上限为1,000,000 entries及64 MiB owned payload，调用方不可覆盖且禁止filesystem spill，等于上限成功、下一次reservation在分配和首次writer poll前typed失败。Exporter不保留完整event prefix或trace bytes，输出阶段仍只保留一页source、一个packet和reusable buffer。Timestamp 一律是 explicit `BUILTIN_CLOCK_TRACE_FILE=11` 下的 Run-relative `elapsed_ns`，必须可被 Trace Processor 的 signed 64-bit nanosecond 表示；descriptor 无 timestamp。V1 只用 direct non-interned TrackEvent slice/instant/counter/flow/debug-annotation fields，并仅使用固定 `trusted_packet_sequence_id=1`；不使用 incremental state/timestamp、interning、compression、custom extension、legacy event 或 unstable Chrome/Android field。 |
| D52 | Exporter 对 captured prefix 中规范化的 typed track 与 causal-link identity 排序，并分配 dense nonzero export-local track UUID/flow ID；canonical identity 同时保留在 annotation，ID-space exhaustion 明确失败。Descriptor parent-before-child，Actor 只建模为 logical group；不能在同一 Perfetto track 合法嵌套的 span 使用确定性 sibling lane，open span 不伪造 end。Exact int64 或 finite exactly-representable double 才可投影 counter；其他大整数/Decimal 在所属 scope timeline 上保留 canonical decimal text 并标记 `counter_projection=not_exact`，绝不把 instant 写入 counter track；missing usage 保持 absent，不能取整、截断或写成零。 |
| D53 | Trace 内置确定性 Troupe metadata，至少包含 exporter/event schema、Run ID、captured watermark、Troupe version、outcome/clean-shutdown availability 和 content warning；不得依赖 v57.2 unstable `TraceAttributes` 承载必要身份。Invalid span/reference、ID/timestamp/numeric/resource overflow、protobuf encode 或 output write/sync/rename failure 都只使 on-demand dump exit 1，不影响 active Production/archive。Local publication 必须如实返回 `published`、`not_published` 或 `publication_indeterminate`：只有 durable success 或 identity-checked durable rollback 才能断言目标状态，无法证明 rollback/namespace durability 时保留现场并要求人工检查，不能虚构旧目标未变。Release 只硬性禁止新增 Python runtime dependency、loose wheel member、external executable/shared library/ELF `DT_NEEDED` 和 runtime service/tool requirement；CI 仅记录 exporter 前后的 wheel/native-module 体积，不设固定 byte gate。 |
| D54 | Perfetto compatibility release gate 包含三层：由独立 protobuf implementation decode 的 byte-exact golden；按 release SHA-256 固定的 official v57.2 `trace_processor_shell` SQL assertions；以及 pinned official Perfetto UI browser screenshot/pixel smoke。Fixture 覆盖 empty/open/nested/multi-Cue/non-nested overlap、equal timestamp、Unicode、annotation/gap/flow/counter、numeric/ID boundary、malformed reference、active/archive watermark 和 repeated deterministic dump。工具只存在于 dedicated CI job，不进入 wheel；current public `ui.perfetto.dev` 只作为 non-blocking scheduled canary，网络或 upstream UI 变化不决定 release correctness。 |

上表只冻结已经讨论确认的产品边界。本文标记为“建议”或“待决”的内容不是 Accepted
contract，后续可以修改而不构成对上述决策的推翻。

## 3. 总体架构

```text
Production / Scene / Cue / Effect ─┐
                                   │
Actor session / Act / tool ────────┼──> DiagnosticHub
                                   │          │
Python annotations ────────────────┘          ├──> hot state / durable event store
                                              ├──> HTTP query + live stream
                                              ├──> per-Act Python DiagnosticSink
Python ViewSpec ──────────────────────────────┤              │
                                              │        Web UI / CLI
                                              └──> Perfetto exporter
                                                        │
                                                   local .pftrace
```

`DiagnosticHub` 是一个 Runtime Run 内唯一的诊断汇聚点。各子系统只产生 normalized
Troupe events，不感知 Web UI、CLI、Python sink 或 Perfetto。实时 UI、查询 API、per-Act sink
和 exporter 都从同一事件序列构建结果，因此任一消费者都不能成为 Runtime 行为的事实来源。

Hub 是 Runtime 内部能力，不是 HTTP server 的一部分。完整 Production Runtime 不允许禁用 Web
server、registry 或持久化，core diagnostic pipeline 失效会使整个 Runtime 失败。独立使用
`Actor.act(..., diagnostic_sink=...)` 的 API 场景仍只需要 in-process producer/fan-out，不强制启动
Production diagnostic server。“同一事件源”表示消费者共享 identity、sequence、timestamp、归一化
kind 和 causality；按照各自冻结的 capture/resource policy，它们可以收到同一事件的不同内容投影，
而不是完全相同的敏感 payload。

这里的“独立使用”只描述 diagnostics deployment profile，不放宽
`docs/design/actor-agent-session.md` 的 Actor authority：`act()` 仍必须处于合法 active RunBinding 和
cued scope。若该 binding 不属于完整 Production diagnostic Runtime 且调用显式提供 sink，Runtime 为该
binding 建立一个 bounded、volatile、in-process hub；同一 binding 的 Act 共享 Run identity、monotonic
origin 和 dense sequence，但不启动 server、不发布 registry、不创建 SQLite，也不访问 `.troupe`。
`diagnostic_sink=None` 不创建该 hub。完整 Production Runtime 仍必须使用 durable profile，任何 startup
或运行故障都不能降级到这个 volatile profile。

若 Actor 的 agent session 在显式 sink 之前已经建立，Runtime 必须在 Act 成功 admission 后、prompt
submission 前把 bind-time frozen 的 per-turn diagnostic context 挂到既有 session control；不能要求重建
session，也不能丢失该 Act 后续的 message、plan、tool、result、context 或 terminal usage observation。
这个 context 同时携带 sink-only tool capture policy：完整 Production profile 复用已有 Run 级 observer
进入同一 hub，但仍为该 turn 注册 opt-in payload sidecar；volatile profile 才提供 per-turn observer
destination。per-Act sink 不得覆盖 Run observer、安装第二个事实源或使 opt-in payload 进入 store/Web。

目标所有权是“一次 Runtime Run 对应一个 hub、一个 server 和一个 `run_id`”。server 与 Runtime 在
同一个 OS 进程中，由 Runtime supervisor 管理，但其 I/O execution context 必须与 Production event
loop 隔离，使 Production loop 被同步代码暂时阻塞时页面仍能响应。Production 对象只声明 annotations
和 views，不拥有 server 生命周期。这样诊断可以覆盖 Production import、构造、`start()`、每次
`scene()`、`stop()` 以及 agent shutdown，而不受 Production 用户代码是否成功启动影响。

当前 CLI 在 `load_production()` 后才创建 `RuntimeCore`。要覆盖构造期间的 Actor cast 和 agent
session opening，正式实现需要将目标启动顺序调整为：

```text
解析并验证 Production package 路径
  -> 创建并真实写入验证 <production-root>/.troupe/
  -> 校验 bind/port/advertise URL 配置
  -> 创建 run identity、hub 和持久化 Run store
  -> 完成 store 写入健康检查
  -> bind 并启动 diagnostic server
  -> 原子发布 registry，并将 diagnostic core 标记为 ready
  -> import / 构造 Production
  -> start / scene / stop / agent shutdown
  -> 固化最终水位并完成 server 收尾
```

第一步只完成 CLI 输入和 Production root 的语法、存在性及 state-root 定位验证；此时 hub 尚不存在，
因此不产生也不事后补写 canonical event。Core ready 后，loader 对实际 package/class 做 resolution、
import 和 construct；`production.path_resolution` span 只包裹这次真实的 ready 后 resolution，start 不得
回填到 monotonic origin 之前。这样既保留 pre-import diagnostics 启动约束，也不伪造不可观测的历史时间。

在 core ready 之前发生任何错误，都必须关闭已经创建的 listener/store 等部分资源，并在执行任何
Production 用户代码前以启动失败结束。运行中 listener/server execution context、canonical event
admission 或持久化 writer 被 supervisor 判定失效时，Runtime 停止接收新工作，取消/收束当前
Production，并以非零状态结束；尽可能记录 fatal diagnostic，但不能依赖已经失效的路径完成记录。
浏览器断开、单个 HTTP request 失败或慢客户端不等于 server 失效。Production 结束后的 drain、server
关闭和 archive lifecycle 按 D28 与第 9.1 节执行。

## 4. Diagnostic Event 模型

### 4.1 事实源

Troupe event 是 append-only、immutable 的领域事实。`DiagnosticEvent` 的逻辑模型是一个 closed
discriminated union；14 个 variant 共用下面的 envelope：

```python
@dataclass(frozen=True, slots=True)
class DiagnosticEventHeader:
    schema_version: Literal[1]
    run_id: UUID
    sequence: int
    elapsed_ns: int
    scope: DiagnosticScope
    caused_by: tuple[CausalLink, ...]
```

逻辑整数 `sequence/elapsed_ns/source_sequence/session_generation` 都是非负 `u64`；Python 中表现为受
范围校验的 `int`。`sequence` 从 1 开始，在一个 Run 内按 hub 接受事实的顺序严格递增。HTTP JSON wire
按 D24 和第 6.1 节将所有 schema-declared `u64` 编码为 canonical decimal string，不能依赖 JavaScript
`number` 表示。`run_id` 是 canonical UUID，`(run_id, sequence)` 已经是 event identity，因此 V1 不再
增加 `event_id` 或 Act-local `act_sequence`。

`elapsed_ns` 是相对 Run monotonic origin 的时间，不是 Unix timestamp。Run metadata 单独保存
wall-clock anchor，所有排序和 duration 只使用 monotonic 值；wall clock 调整不能改变 trace。

`DiagnosticScope` 是每条 event 的完整、immutable 领域归属快照：

```python
@dataclass(frozen=True, slots=True)
class DiagnosticScope:
    scene_id: RunLocalId | None = None
    actor_id: RunLocalId | None = None
    cue_id: RunLocalId | None = None
    effect_id: RunLocalId | None = None
    act_id: RunLocalId | None = None
    tool_call_id: RunLocalId | None = None
    session_generation: int | None = None
```

`RunLocalId` 是由 diagnostics 分配的非空、opaque ASCII string，只在一个 `run_id` 内解释；display
name、Python object address 和 provider raw ID 都不是 identity。已有 Runtime identity 可以映射到
它，但 Actor 的进程内地址不能泄漏到事件。缺失的 scope 字段保持 `None`，不能用空字符串或 0
表示 unknown。scope 表示 Scene/Actor/Cue/Act 等领域归属，不表示时间嵌套。

`caused_by` 是一个有界的 backward-link tuple。每个 `CausalLink` 含同一 Run 内严格小于当前
`sequence` 的 `source_sequence`，以及 closed relation：`dispatch`、`return`、`handoff`、`retry` 或
`follows_from`。一个事件可以有多个原因；不再使用含义模糊的单一 `causality_id`。找不到可靠来源时
tuple 为空，不能猜测因果。

### 4.2 Closed event taxonomy

V1 union 固定为：

```python
from typing import TypeAlias


DiagnosticEvent: TypeAlias = (
    SpanStarted
    | SpanFinished
    | InstantOccurred
    | CounterSampled
    | AgentMessageDelta
    | AgentMessageCompleted
    | AgentPlanSnapshot
    | ContextUsageSampled
    | ActTokenUsageFinalized
    | ObservationGap
    | CustomSpanStarted
    | CustomSpanFinished
    | CustomInstantOccurred
    | CustomCounterSampled
)
```

每个 class 都直接携带公共 header 字段和唯一的 snake-case `kind` discriminator；不存在外围
`ActDiagnosticEvent` wrapper。14 个 literal 依 union 顺序分别为 `span_started`、`span_finished`、
`instant_occurred`、`counter_sampled`、`agent_message_delta`、`agent_message_completed`、
`agent_plan_snapshot`、`context_usage_sampled`、`act_token_usage_finalized`、`observation_gap`、
`custom_span_started`、`custom_span_finished`、`custom_instant_occurred` 和
`custom_counter_sampled`。各 variant 的 V1 payload 如下：

| Variant | V1 payload 与更新语义 |
|---|---|
| `SpanStarted` | closed `span_kind`、可选 `parent_span_id` 和该 kind 的 typed start detail；当前 event 的 `sequence` 同时成为 `span_id` |
| `SpanFinished` | `span_id`、`outcome=completed/cancelled/failed`、可选 stable `error_code`；不重复 start detail |
| `InstantOccurred` | closed `instant_kind`、可选 containing `span_id` 和由 kind 决定的 typed detail |
| `CounterSampled` | closed `counter_kind` 和非负 `u64 value`；unit 由 kind 定义，不携带计算后的 display value |
| `AgentMessageDelta` | stable `message_id` 和 append-only UTF-8 `text_delta`；只含 user-visible agent output |
| `AgentMessageCompleted` | `message_id`、最终 `utf8_bytes/unicode_scalar_count` 和 source-capture `truncated`；不重复完整正文 |
| `AgentPlanSnapshot` | 替换前一 snapshot 的有序 `entries(content, priority, status)`；不把 delta 当 snapshot |
| `ContextUsageSampled` | `context_used_tokens/context_window_tokens: u64` 和可选成对的 session `cumulative_cost_amount: DecimalString` / `cumulative_cost_currency: ISO4217`；occupancy ratio 由 consumer 计算 |
| `ActTokenUsageFinalized` | `availability`、provider total/input/output、可选 thought/cache 分类和 validated `source`；遵守本节 Usage 语义 |
| `ObservationGap` | producer/component、reason、已知 dropped count、受影响的 elapsed interval/kind/scope；表示 canonical source 形成前已知有事实丢失 |
| `CustomSpanStarted` | bounded namespaced `name`、可选 `parent_span_id` 和 bounded structured attributes；其 `sequence` 成为 span ID |
| `CustomSpanFinished` | `span_id`、terminal outcome 和 bounded terminal attributes；不重复 start attributes |
| `CustomInstantOccurred` | bounded namespaced `name`、可选 containing `span_id`、可选 closed severity 和 bounded attributes |
| `CustomCounterSampled` | bounded namespaced `name`、finite integer/decimal value、bounded unit 和 bounded dimensions |

所有内置 `span_kind/instant_kind/counter_kind` 都是 V1 closed enum，而不是任意字符串。V1 的完整
provider-neutral kind 列表如下；增加 built-in kind 需要新的 negotiated event schema version（或未来
另行冻结的兼容机制），不能在 V1 中静默添加，也不能用 provider raw kind 穿透：

| Family | Built-in kinds |
|---|---|
| Span | `run.lifecycle`, `production.path_resolution`, `production.load`, `production.construct`, `production.start`, `production.stop`, `production.shutdown`, `scene.lifecycle`, `scene.drain`, `scene.cleanup`, `actor.handle_lifetime`, `cue.mailbox_wait`, `cue.execution`, `effect.lifecycle`, `agent.session.opening`, `agent.session.lifecycle`, `agent.session.closing`, `act.lifecycle`, `act.caller`, `agent.turn`, `agent.thinking`, `tool.call` |
| Instant | `actor.cast`, `cue.admitted`, `cue.enqueued`, `cue.dispatched`, `cue.cancel_requested`, `effect.created`, `effect.returned`, `effect.consumed`, `agent.session.ready`, `agent.session.broken`, `act.admitted`, `act.waiting_ready`, `act.prompt_submitted`, `act.cancel_requested`, `act.supervisor_handoff`, `agent.turn.activity`, `agent.turn.terminal`, `agent.turn.settled`, `tool.updated`, `result.submitted`, `result.rejected`, `result.repair_requested`, `result.accepted`, `result.missing`, `diagnostic.component_failed` |
| Counter | `actor.mailbox_depth`, `cue.active`, `agent.turn.active`, `result.validation_rejections`, `diagnostic.dropped_events` |

内置 detail 同样是按 kind 的 closed typed union。它只携带显示和诊断所需的稳定字段，例如 Actor
display name/type、Effect type、provider/effective model/effort、tool title/kind/status、result issue
code/path 和 normalized error code；原始 provider JSON 不能进入 detail。`severity` 不是公共 header：
built-in outcome/error 决定展示级别，只有 custom instant 可以显式提供 closed severity。

`diagnostic.component_failed` 的 sink-delivery detail 冻结为
`component="sink"`、Run-local `component_id`、`stage="enqueue"|"callback"`、
`error_code="delivery_queue_unavailable"|"callback_raised"|"callback_invalid_return"` 和可选
`related_event_sequence`；不含 traceback、exception object 或 provider/user payload。Unexpected enqueue
channel failure及callback第一次进入failed state各自产生恰好一个该instant。普通bounded queue淘汰或容量耗尽
不伪装成component failure：它只更新Act/sink scoped cumulative `diagnostic.dropped_events` counter和delivery
summary。所有sink-targeted component failure只进入Run store/Web/CLI，不投影给任何per-Act sink，避免
callback failure递归产生新的callback traffic。

Custom variant 与 built-in variant 分开，避免 Python extension 扩大内置 enum。Custom name、flat
attributes/dimensions、numeric normalization、namespace collision 和精确 resource limits 由 D41-D42 及
第 8.1 节定义；没有独立 namespace registry 或 nested arbitrary JSON。

`DecimalString` 是规范化的有限十进制定点字符串；金额与 custom decimal 在逻辑模型和 wire model
中都不能降为 binary floating point。Cost amount/currency 必须同时存在或同时缺失。所有 optional
usage 值保持 absent/`None`，不能用 0 表示 unknown；provider 直接报告的 0 仍是有效 observation。

#### Span 与因果编码

所有 span 都采用 append-only start/finish pair：

```text
120 SpanStarted   span_kind=act.lifecycle
121 SpanStarted   span_kind=act.caller       parent_span_id=120
124 Instant       instant_kind=act.prompt_submitted
125 SpanStarted   span_kind=agent.turn       parent_span_id=120 caused_by=(124, dispatch)
180 SpanFinished  span_id=121 outcome=cancelled
181 Instant       instant_kind=act.supervisor_handoff
230 SpanFinished  span_id=125 outcome=completed
233 ActTokenUsageFinalized
234 SpanFinished  span_id=120 outcome=completed
```

`span_id` 就是对应 `SpanStarted` 或 `CustomSpanStarted` 的 `sequence`，不使用第二个 allocator。每个
start 最多有一个 matching finish；finish 必须引用同 Run 中更早、类型匹配且尚未结束的 start。运行中
只有 start event，UI 将右边界画到 now；结束时追加 finish，不能回写 start 或生成一条 mutable
completed-span record。

`parent_span_id` 只表示严格时间包含：child 不能早于 parent 开始；两者结束后 child 不能晚于 parent。
Scene/Actor/Cue/Act 的组织由 `scope` 表示，跨 task、mailbox、Effect return、retry 与 cancellation
handoff 使用 `caused_by`。因此一个 Act 的 caller 与 remote turn 都可以是 `act.lifecycle` 的 child，
但 remote turn 不是 caller 的 child；如果 Act 可能在 Cue caller 结束后继续 settlement，Act 也不能
伪装成 Cue execution 的 temporal child。

`ObservationGap` 是 canonical、全局有序的事实，表示 instrumentation/normalization/hub 接受之前
已知丢失了 observation。丢失事实若尚未分配 sequence，gap 只报告 count/time/scope，不能伪造 event
identity。浏览器、CLI 或 Python sink 自己落后时，原 canonical event 仍然存在，因此只在相应
transport/summary 中报告 subscriber-local delivery loss，不能向 Run stream 插入
`ObservationGap`。

### 4.3 覆盖范围

Runtime/Production 侧至少产生：

- Production path resolution、load、construct、`start`、每次 `scene`、`stop`、shutdown。
- Actor cast、handle lifetime 与 agent session lifecycle。
- Cue admitted/enqueued、mailbox wait、`cued()` execution、completion/cancellation/failure。
- Effect creation、return 和 Scene consumption 的 identity 与因果关系。
- Scene drain、cancellation propagation 和 cleanup。
- queue depth、active Cues、active turns、validation rejection 等 counter。

`Actor.act()` 侧至少产生 provider-neutral 的：

- Session opening attempt、ready、broken、closing、closed。
- Act caller waiting-ready、submitted、cancel requested、supervisor handoff、completed/failed。
- Remote agent turn start、activity、terminal event、authoritative settlement。
- 面向用户的 agent message chunk 与 message boundary，以及 plan snapshot/progress。
- Tool start/end、result submission、validation rejection、repair、accepted/missing。
- Context occupancy、per-Act final token accounting、provider 能提供的 cumulative cost、effective
  model/effort 和 normalized error code。

普通 ACP update 不应原样长期保存。Adapter 完成协议校验和 provider normalization 后，只向 hub
发稳定的 Troupe event；原始 update 仍按现有 agent-session contract 分类后丢弃。`AgentMessageChunk`
中的 user-visible message content 属于正式 diagnostic payload；`AgentThoughtChunk` 只产生不含内容的
thinking activity span/counter，其 reasoning content 直接释放。

### 4.4 Caller 与远端 turn

`Actor.act()` caller span 和 remote agent turn span 必须分开。现有取消语义允许 caller 在
supervisor handoff 后先结束，而原远端 turn 继续到 authoritative settlement：

```text
Act lifecycle ──────────────────────────────────────┐
├── Actor.act() caller ───────────────x cancelled   │
└── agent turn ───────────────────────── settlement │
                         \_____ handoff flow ________/
```

如果 UI 或 exporter 把两者合并成一个 span，就会错误表达 cancellation、资源占用和 session
可复用状态。Act lifecycle 是二者共同的 temporal parent；Scene、Actor、Cue 归属来自 scope。因为
remote turn 可能在 Cue execution 已结束后继续，Act lifecycle 不能无条件作为 Cue execution 的 child。

### 4.5 `Actor.act()` diagnostic producer 与 Python sink

Agent runtime 先产生一份 normalized event，再由 Run 级 `DiagnosticHub` 分发给持久化/Web 路径和
当前 Act 可选的 Python `DiagnosticSink`。传入 sink 不能替代、关闭或改变 Run 级 diagnostics；
没有传入 sink 也不能改变全局 Web/CLI 可见性。

Public API 目标形状：

```python
result = await self.act(
    script="inspect the repository",
    output_schema=result_schema,
    diagnostic_sink=EvaluationSink(
        capture=DiagnosticCapture(tool_inputs=True, tool_outputs=True),
    ),
)
```

```python
class Actor:
    async def act(
        self,
        *,
        script: str,
        output_schema: dict[str, act_schema.FieldSpec],
        diagnostic_sink: DiagnosticSink | None = None,
    ) -> dict[str, JsonValue]:
        ...
```

`DiagnosticSink` 与相关 event/capture/summary 类型属于 public `troupe.diagnostics` 模块。
`diagnostic_sink` 是 keyword-only，默认 `None`，并在同步 preflight 验证为 `DiagnosticSink` 的实例。
真正 await `_ActCall` 后，Runtime 先验证 active Cue context 并取得 Actor admission；只有成功 admission
才原子绑定 sink、分配 `act_id` 并发出 `SpanStarted(span_kind="act.lifecycle")`。因此
schema/context/busy 等 pre-start failure 不消耗 sink，创建但从未 await 的 `_ActCall` 也不形成
subscription。

同一个 sink object 第一版只能成功绑定一次，不能并发或依次复用于另一个 Act。已绑定 sink 由
Runtime 持有到 delivery terminal，不能依赖调用方继续保留引用。`DiagnosticSink` 的 public lifecycle
固定为 `UNBOUND -> BOUND -> SEALED -> CLOSED`；callback 或 delivery failure 是独立的 latched delivery
状态，会停止 callback，但 sink 仍在 Act terminal 后依次 seal/close 并形成 summary，而不是增加另一条
public lifecycle。子类必须调用 `super().__init__()`；Runtime 在同步 preflight 检查 base initialization，
在第二个 Act 提交 prompt 前拒绝已经绑定过的 object。

基类采用一个有序入口，而不是为每种事件持续增加 callback method：

```python
from abc import ABC, abstractmethod
from collections.abc import Awaitable


class DiagnosticSink(ABC):
    def __init__(self, *, capture: DiagnosticCapture | None = None) -> None:
        ...

    @abstractmethod
    def on_event(
        self,
        event: DiagnosticEvent,
        /,
    ) -> None | Awaitable[None]:
        ...

    async def wait_closed(self) -> DiagnosticSinkSummary:
        ...
```

状态误用使用一个 public error family：

```python
class DiagnosticSinkStateError(RuntimeError):
    code: Literal["uninitialized", "unbound", "already_bound"]
```

未调用 base initializer 为 `uninitialized`；尚未成功绑定就调用 `wait_closed()` 为 `unbound`；已经
绑定或结束后再次用于另一个 Act 为 `already_bound`。这三个 error 都是调用方 API misuse，不进入
agent failure、Act outcome 或 diagnostic completeness。

`DiagnosticSink` 基类负责绑定状态、内部有界队列、串行 dispatcher 和 completion future；子类只实现
`on_event()`。普通 `def` 适合只做有界内存聚合，`async def` 可以执行异步工作；dispatcher 对返回的
awaitable 串行 await，两种形式都不能在 agent hot path 执行。回调收到的是 Run 事实流所使用的同一
immutable、closed、versioned `DiagnosticEvent` hierarchy 中与当前 Act 相关的事件，不是单独的
`ActDiagnosticEvent` wrapper 或重新解释后的类型。该公共 hierarchy 中 sink 的 Act-relevant
投影为：

| Event | 关键字段与语义 |
|---|---|
| `SpanStarted/SpanFinished` | `act.lifecycle`、`act.caller`、`agent.turn/thinking` 和 `tool.call`；caller 与 turn 分开结束 |
| `InstantOccurred` | Act admission/waiting/submission/cancellation/handoff、turn activity/settlement、tool update 和 result validation transition |
| `CounterSampled` | 当前 Act scope 内的 active/rejection/drop 等 built-in counter sample |
| `AgentMessageDelta` | stable message ID、append-only text delta；这是 user-visible output，不是 thought/reasoning |
| `AgentMessageCompleted` | message ID、最终 byte/character count、是否 truncated；不重复携带完整文本 |
| `AgentPlanSnapshot` | 当前完整 plan snapshot；provider 未提供 plan 时不合成 |
| `ContextUsageSampled` | session scope 的 `context_used_tokens/context_window_tokens` 和可选 cumulative cost |
| `ActTokenUsageFinalized` | 当前 Act 的 terminal token accounting；包含 availability、provider total/input/output/thought/cache 字段和 source |
| `ObservationGap` | 当前 Act 的 canonical observation 在 hub 接受前已发生已知丢失 |
| `Custom*` | 在当前 Act scope 中由 Python instrumentation 产生且满足当前 sink capture policy 的 custom event |

`on_event()` 对一个 sink 严格串行，并按全局 `sequence` 递增调用；不会并发进入同一个 Python
object，也不另造 `act_sequence`。typed event 不暴露 provider raw JSON，provider-specific 字段只能
进入有界、显式 namespaced metadata。由于 sink 只收到当前 Act 的投影，全局 `sequence` 跳号通常
表示其他 Production event，不代表丢失。匹配当前 scope 的 `ObservationGap` 表示事实源不完整；
sink 自身队列丢失只通过 final `DiagnosticSinkSummary.complete=False` 与 dropped count 表示。

ACP `ContentChunk.message_id` 是 optional。Troupe event 的 `message_id` 必须始终存在：provider 提供
ID 时映射到 Run-local opaque ID；缺失时由 pinned adapter/normalizer 合成，并保留
`source_message_id=None`。一个 Act 内所有缺失 ID 的 user-visible chunk 共用一个 anonymous synthetic
message，即使中间穿插 tool、plan、usage 或不采集的 reasoning；不能按文本、时间间隔或 interleave
猜新 message。显式 provider ID 发生变化时先完成上一条显式 message；anonymous 与显式 message 可以
同时 open，turn terminal 时按首次出现顺序为全部 open message 发出 `AgentMessageCompleted`。异常 source
termination 或 resource truncation 使 completion 的 `truncated=True`。已经完成的 provider ID 被复用时
分配新的 Run-local message ID 并发出 `ObservationGap`，但不改变 agent correctness outcome。

连续文本的合并属于 canonical normalization，而不是 sink-local 优化。Normalizer 只合并相邻且
normalized message ID 相同的 text chunk，在累计 16 KiB、首 chunk 经过 20 ms、同 Act 出现其他
canonical event 或 turn terminal 时 flush；`elapsed_ns` 使用第一段 chunk 的 observation time，空文本
不产生 event。Sequence 一旦分配，store、Web、SSE 和 Python sink 都不得继续合并或改写该 event。

`DiagnosticCapture` 是 immutable、slotted、keyword-only public value，在 sink bind 时冻结：

```python
@dataclass(frozen=True, slots=True, kw_only=True)
class DiagnosticCapture:
    agent_messages: bool = True
    plans: bool = True
    tool_calls: bool = True
    result_validation: bool = True
    usage: bool = True
    custom_events: bool = True
    tool_inputs: bool = False
    tool_outputs: bool = False
```

所有字段必须是实际 `bool`，不接受 `0/1`。过滤是下面这张 closed matrix，不允许 adapter、sink 或
event detail 自行猜测归属：

| Capture field | 控制的 event kind/detail |
|---|---|
| 不可关闭 | `act.lifecycle`、`act.caller`、`agent.turn` 的 start/finish；Act admission/waiting/submission/cancellation/handoff 和 turn activity/terminal/settled instant；影响当前 Act 的 `ObservationGap` |
| `agent_messages` | `AgentMessageDelta`、`AgentMessageCompleted`和不含thought content的`agent.thinking` span start/finish |
| `plans` | `AgentPlanSnapshot` |
| `tool_calls` | `tool.call` span start/finish 与 `tool.updated` instant 的 metadata |
| `result_validation` | `result.submitted/rejected/repair_requested/accepted/missing` instant 和 `result.validation_rejections` counter |
| `usage` | `ContextUsageSampled`（含可选累计cost pair）和`ActTokenUsageFinalized`；`False`时两者不投递给该sink，但canonical event仍写入Run |
| `custom_events` | 四个 `Custom*` variant |
| `tool_inputs` / `tool_outputs` | 不控制 tool event 是否投递，只控制其 `captured_input` / `captured_output`；任一为 `True` 都要求 `tool_calls=True` |

Act scope 内的 `agent.turn.active` 与 `diagnostic.dropped_events` counter 属于不可关闭的 lifecycle/delivery
evidence；不属于当前 Act scope 的 mailbox/Cue counters 不投影给 per-Act sink。Thinking activity明确随
`agent_messages`，context occupancy明确随`usage`；不存在`context`或`thinking`之类未声明的隐式flag。
`tool_inputs/tool_outputs=True`且`tool_calls=False`在构造时抛
`ValueError`。`result_validation` 包含表中冻结的submitted/rejected/repair/accepted/missing transition
metadata，但不包含submitted、invalid或validated result的值/payload。

默认包含agent message/thinking activity、plan、context/terminal usage、tool name/title/status、result
validation metadata和custom event；tool input/output默认关闭，但evaluation sink可以分别显式开启。Opt-in tool capture 包含 ACP
stable `raw_input`、`raw_output`、content 和 locations，排除 protocol envelope 与 `_meta`，并且只投影给
该 sink，不会自动进入 Run store、Web UI 或 Perfetto。来源选择后的 payload 视为 opaque：Troupe
不检查、识别、按 key 脱敏或改写内容；是否允许 tool 暴露 credential、文件内容或其他敏感信息，由
tool、agent 和启用 capture 的调用方负责。Agent thought/reasoning、script 和 validated result value
没有 opt-in。

Resource policy 固定为：tool typed payload 最大 depth 32、nodes 65,536，每个 input/output snapshot 的
canonical encoding 最大 256 KiB，每 Act 的 tool payload 合计最大 4 MiB；user-visible agent message
最大每条 4 MiB、每 Act 16 MiB；plan snapshot 最大 256 KiB。结构化字段超限时整个字段原子省略，
不能产生非法 partial JSON；streamed text 达限后停止后续 delta。两者都在相应 terminal/event 上显式
标记 truncation，并使请求了该内容的 sink summary `complete=False`。

Python public tool capture 使用同一个 `DiagnosticEvent` hierarchy 中 tool start/update detail 的两个
optional field，不创建 sink-only event subclass：

```python
@dataclass(frozen=True, slots=True, kw_only=True)
class FrozenJsonArray:
    items: tuple["FrozenJsonValue", ...]


@dataclass(frozen=True, slots=True, kw_only=True)
class FrozenJsonObject:
    entries: tuple[tuple[str, "FrozenJsonValue"], ...]


FrozenJsonValue: TypeAlias = (
    None | bool | int | Decimal | str | FrozenJsonArray | FrozenJsonObject
)


@dataclass(frozen=True, slots=True, kw_only=True)
class DiagnosticToolLocation:
    path: str
    line: int | None


@dataclass(frozen=True, slots=True, kw_only=True)
class DiagnosticToolInput:
    raw_input: FrozenJsonValue | None
    truncated: bool


@dataclass(frozen=True, slots=True, kw_only=True)
class DiagnosticToolOutput:
    raw_output: FrozenJsonValue | None
    content: tuple[FrozenJsonValue, ...]
    locations: tuple[DiagnosticToolLocation, ...]
    truncated: bool
```

`FrozenJsonValue` 是 public closed immutable JSON projection；`Decimal` 必须finite且保持canonical decimal
value，object key是string并按canonical key order保存。它不暴露mutable `dict/list`，也不执行object hook。
`DiagnosticToolInput`只由`tool_inputs=True`填入，
`DiagnosticToolOutput` 只由 `tool_outputs=True` 填入；没有选择、来源字段不存在或该方向未出现时相应
`captured_input/captured_output=None`。请求了捕获但字段因单 snapshot 或 per-Act budget 原子省略时仍返回
对应 wrapper 且 `truncated=True`，不能用 `None` 混淆“未请求”和“因预算丢失”。Canonical event、store、
Web、CLI 与 Perfetto 永远不携带这两个字段的内容。

#### Usage 语义

Context occupancy 与 token accounting 是不同指标，不能合并成一个 `tokens` 字段：

- ACP stable `UsageUpdate.used/size` 映射为 `context_used_tokens/context_window_tokens`，表示持久
  session 当前保留在 context 中的 token 数和 context window 总容量。`context_used_tokens` 可能跨多个
  Act 增长，也可能因 compaction 或 truncation 下降；它是当前占用快照，不是 session 历史累计 token，
  也不等于当前 Act 消耗。
- Act lifecycle 开始时，如果 session 已有最近一次 context sample，无论当前 Act 是否传入 sink，都由
  hub 发送一个新的、有序 canonical `ContextUsageSampled`，并用 `sample_origin=carried_forward`、
  `observed_elapsed_ns` 及可用的 `follows_from` link 指向原 observation；不能只为 sink 伪造私有 event，
  也不能让 sink 的存在改变全局事实流。其后 provider update 实时发送，terminal summary 再携带
  latest snapshot。没有 sample 时保持 unavailable，不用旧模型配置推算 window。
- 一个 Troupe Act 对应一个 ACP `session/prompt` agent turn。Act token accounting 覆盖该 turn 内由
  agent 发起的所有 model request，包括 tool loop 和 result repair 前后的请求；它不是最后一条 agent
  message 的文本 token 数，也不是一次底层 model request 的 usage。
- `ActTokenUsageFinalized` 对每个 `SpanStarted(span_kind="act.lifecycle")` 恰好产生一次，并在不再可能
  得到该 Act 的可靠 accounting 后、对应 `SpanFinished` 前发出。已提交 prompt 时通常等待
  authoritative turn settlement；未提交、session terminal 或 provider 不报告时也必须以
  unavailable 结束，不能省略事件或记成零。
- Public terminal payload 精确为：

  ```python
  @dataclass(frozen=True, slots=True, kw_only=True)
  class ActTokenUsageFinalized:
      # DiagnosticEvent common envelope fields are omitted here.
      kind: Literal["act_token_usage_finalized"]
      availability: Literal["available", "partial", "unavailable"]
      source: Literal["acp.prompt_response.usage"] | None
      unavailable_reason: Literal[
          "prompt_not_submitted",
          "source_unsupported",
          "usage_not_reported",
          "turn_settlement_unknown",
      ] | None
      provider_total_tokens: int | None
      input_tokens: int | None
      output_tokens: int | None
      thought_tokens: int | None
      cached_read_tokens: int | None
      cached_write_tokens: int | None
  ```

  所有 present token value 必须是排除 `bool` 的非负 Python `int`。Troupe public contract 不为这些字段
  声明 `u64` 或其他 product maximum；ACP 的底层整数类型只是 adapter implementation detail。Provider
  直接报告的零是有效 observation，`None` 才是 unknown。
- `available` 当且仅当 provider total、input 和 output 三个 primary 字段全部存在；可选 thought/cache
  缺失不降级。`partial` 当且仅当六个数值中至少一个存在但三个 primary 字段不完整；`unavailable` 当且
  仅当六个数值全部为 `None`。有任何数值时 `source="acp.prompt_response.usage"` 且
  `unavailable_reason=None`；完全 unavailable 时 `source=None` 且 reason 必须存在。Runtime 不能产生与
  这些不变量冲突的组合。
- `provider_total_tokens` 保留来源对 total 的定义；provider 分类可能重叠，Troupe 不验证或推断
  `total == input + output`，也不通过 breakdown 合成缺失字段。Thought token 数值不包含或授权
  thought/reasoning content。
- unavailable reason 按事实边界唯一选择：prompt 从未越过 submission boundary 是
  `prompt_not_submitted`；已提交但没有 authoritative turn settlement 是 `turn_settlement_unknown`；已经
  authoritative settlement、但当前 pinned adapter 没有通过 whole-turn usage qualification 是
  `source_unsupported`；qualified carrier 在 authoritative settlement 没有给出任何可信值是
  `usage_not_reported`。若 qualified carrier 给出部分可信值则是 `partial`，不使用 unavailable reason。
- V1 唯一 `source` 是经过验收的 normalized ACP terminal `PromptResponse.usage` carrier。Adapter profile
  必须证明它统计整个当前 agent turn；字段存在或名字叫 `last` 不足以证明 Act scope。Codex/Claude 只有
  通过固定版本的单 request 与 tool-loop/multi-request 验收后才能产生数值；Kimi 在通过等价验收前为
  `source_unsupported`。Agent、model、effort 与 adapter qualification/version 留在已有 typed Act/Run
  metadata，usage event 不重复，也不直接暴露 ACP `Usage` 或 provider raw metadata。
- 当前 ACP stable contract 只保证 `UsageUpdate.used/size` 和可选累计 cost。End-Turn Token Usage 仍是
  Draft，Rust 类型受 `unstable_end_turn_token_usage` feature 控制，所以 per-Act accounting 是
  capability-dependent，不能成为跨 provider evaluation 的必有输入。
- 第一版只接受 agent/provider 直接报告的当前 turn final accounting。Troupe 不用本地 tokenizer
  估算，不通过 context occupancy snapshot 的差值推导，也不通过相邻 session cumulative counter 差分
  归因。未来若增加 estimated/derived 指标，必须使用不同事件和字段，不能填充 authoritative 字段。
- Caller cancellation 或业务 result failure 不代表没有消耗 token。只要 supervisor-owned turn 最终
  报告可靠 accounting，仍归到原 `act_id`；否则为 unavailable。Evaluation 若要求缺失的精确 usage，
  结论必须是 unavailable/inconclusive，而不是零。
- Run/Scene/Actor 对六个数值字段分别聚合 `known_sum: int | None`、`reported_acts: int` 和
  `finalized_acts: int`，并保留 available/partial/unavailable Act count。`known_sum` 只在没有任何 Act
  报告该字段时为 `None`；coverage 不完整时只能标为 known/partial total，不能暗示是整个范围的总消耗。
  Ratio/percentage 由 consumer 从整数 coverage 计算，不存 binary float。聚合使用实现语言的正常安全
  integer sum，不为不现实的天文 token 数额另设产品语义或展开专门的 overflow contract。
- `DiagnosticSinkSummary` 只描述 delivery health，不重复 token 字段，也不保存 usage event pointer。
  `capture.usage=True` 时 evaluator 自行保留收到的 immutable terminal event；完整收到
  `availability="unavailable"` 仍可有 `summary.complete=True`，表示完整证据明确说明 accounting 不可用。

#### Delivery、故障与结束

agent protocol path 只对 Run hub 做非阻塞提交，绝不 await `on_event()`。每个 Runtime 创建一个
daemon thread，并在其上运行专用 diagnostic asyncio loop。每个 sink 是该 loop 上的独立 task：同一
sink 严格按递增 global sequence 串行 callback，不同 sink 可以交错运行。Callback 在空
`contextvars.Context` 中调用，不继承 Actor/Cue/task lineage，因此没有 cued authority，不能调用
`Actor.act()`、`make_effect()` 等 context-bound API。Callback 也不能假设运行在创建 sink 的 event
loop；若要操作原 loop-owned object，必须显式 thread-safe marshal。

每个 sink 的 accepted-but-undelivered budget 固定为 1,024 events 或 8 MiB，先达到者为满；其中保留
32 events/256 KiB，只允许结构与 terminal event 使用。一个 Runtime 的所有 sink delivery 合计固定为
16,384 events 或 64 MiB。Byte accounting 使用当前 sink 投影后的 canonical encoding，并包含正在
callback 的 event。V1 不把这些值暴露为 constructor、Runtime flag 或其他 tuning surface。

队列压力先淘汰低优先级 content/progress：message delta、被更新取代的旧 plan snapshot、context
sample、counter、tool progress/content 和 custom progress。Built-in span start/finish、message
completion、Act usage terminal、result terminal transition 和 `ObservationGap` 优先使用保留容量。
每次淘汰都记录 kind/event/byte count，使 summary 永久 `complete=False`；这是 subscriber-local loss，
不能伪造 canonical `ObservationGap`。保留容量也耗尽时停止该 sink 并以
`close_reason="delivery_overflow"` 收尾；agent、Run hub 和 Production 继续运行。

Callback 必须返回 `None`，或返回最终 resolve 为 `None` 的 awaitable。抛出任何 exception、自发抛出
`CancelledError`，或同步/异步返回非 `None`，都停止后续 callback 并形成：

```python
@dataclass(frozen=True, slots=True)
class DiagnosticCallbackFailure:
    kind: Literal["raised", "invalid_return"]
    event_sequence: int
    exception_type: str | None
    message: str | None
    message_truncated: bool
```

Runtime 只保存有界 exception type/message，不保留 traceback 或原 exception object。Callback fault
不从 `act()` 或 `wait_closed()` 重抛，不改变 agent/session 状态；Run hub另发一个不进入任何per-Act sink的
`InstantOccurred(instant_kind="diagnostic.component_failed", component="sink", stage="callback")`。失败sink仍等待
Act terminal，以便summary给出最终Act outcome。普通queue淘汰/overflow不发该instant；其cumulative
`diagnostic.dropped_events` counter与summary drop fields是唯一事实。

`act()` 返回、抛错或被取消时不等待慢 sink drain。Terminal 顺序固定为 B17 先把唯一
`ActTokenUsageFinalized` admission 完成，B05 再生成 `SpanFinished(span_kind="act.lifecycle")`，B18 按
B15 capture matrix 投影这两个事实并把Act finish送入sink queue，B14随后使该Act的task authority全局
过期，最后 B16 才能 seal/retire；即使
`usage=False` 过滤了 usage event，也必须先完成 canonical usage admission，再投递 Act finish、再 seal。
Runtime 在当前 `SpanFinished(span_kind="act.lifecycle")` 入队后 seal queue；正常 `wait_closed()` 要等到 queue 排空且
最后一个成功 callback 返回。该方法没有 timeout 参数，可以在绑定后重复 await，并始终返回同一个
immutable summary；waiter 自身被取消只取消该次等待，不能取消 sink 或 Act，调用方可在外层使用
`asyncio.wait_for()`。未绑定时立即抛 `DiagnosticSinkStateError(code="unbound")`。V1 不提供 public
`close()` 或 force-cancel，因为 Python 无法可靠终止同步阻塞 callback。

Public summary 形状为：

```python
@dataclass(frozen=True, slots=True)
class DiagnosticDropCount:
    event_kind: str
    events: int
    encoded_bytes: int


@dataclass(frozen=True, slots=True)
class DiagnosticSinkSummary:
    run_id: UUID
    act_id: RunLocalId
    act_outcome: Literal["completed", "cancelled", "failed"] | None
    close_reason: Literal[
        "act_finished",
        "callback_failed",
        "delivery_overflow",
        "runtime_shutdown",
    ]
    complete: bool
    delivered_events: int
    first_delivered_sequence: int | None
    last_delivered_sequence: int | None
    dropped_events: int
    dropped_bytes: int
    dropped_by_kind: tuple[DiagnosticDropCount, ...]
    source_gaps: int
    truncated_payloads: int
    callback_failure: DiagnosticCallbackFailure | None
    callback_abandoned: bool
```

`complete` 只回答“当前 capture 要求的证据是否完整”，不回答 Act 是否成功。相关或 impact unknown 的
source gap、subscriber-local drop、请求内容的 resource truncation、callback failure 或 Runtime
shutdown 截断都使其为 false；capture 主动关闭某类事件、Act failed/cancelled、provider 没有 plan，
或明确报告 usage unavailable 都不使其为 false。按 D35/D40，summary 不重复终态 token accounting；
完整交付的 unavailable event 与 delivery incomplete 是两个独立事实。

Production shutdown 使用现有有限 shutdown deadline，不为 sink 单独延期：先 seal，尽最大努力交付
terminal/gap，再丢弃 pending queue 并取消 async callback。Runtime 发起的取消不是 callback failure。
无法终止的同步 callback 不阻塞 Runtime shutdown；summary 以 `runtime_shutdown`、`complete=False`、
`callback_abandoned=True` 收尾。这里的 closed 表示 Runtime 已停止 delivery，不承诺任意用户同步代码
已经返回。

独立 diagnostic loop 隔离了 Production asyncio scheduling，但 Python callback 仍共享进程的 GIL、CPU
和内存；同步阻塞 callback 也会阻塞同一 diagnostic loop 上的其他 sink。需要强隔离或不希望 evaluation
扰动被测对象时，应通过 diagnostic server 在独立进程中消费事件。

这条旁路必须满足：

- 不改变 `act()` 的返回值、exception、settlement、cancellation 或 session state machine。
- 不允许 Actor 业务代码通过返回值依赖 provider-specific usage 或 trace metadata。
- hot path 不同步执行或 await 用户 Python，也不因观察者慢而阻塞 agent protocol dispatch；专用 thread
  不承诺隔离 GIL、CPU 或内存竞争。
- sink/subscriber 故障不能被伪装成 agent turn failure；如果该旁路发生观测缺口，必须通过 gap/drop
  状态呈现。Production diagnostic core 的 fatal failure 遵守第 3、5、9、10 节的 Runtime 终止语义。

`DiagnosticSink` 可以支撑记录、评分、告警和离线 evaluation，但第一版只支持观察型 evaluation。
如果 evaluation 要拒绝 result、要求 agent repair、取消 turn 或决定 `act()` 是否成功，那是新的
evaluator/policy correctness API，必须与 output schema、错误、超时和重试语义一起独立设计，不能
通过 sink callback 的返回值、异常或阻塞隐式实现。

典型的完整证据 evaluation 显式等待 delivery，而不是让 `act()` 隐式等待：

```python
sink = EvaluationSink()
result = await self.act(
    script="inspect the repository",
    output_schema=result_schema,
    diagnostic_sink=sink,
)
summary = await sink.wait_closed()
if not summary.complete:
    raise EvaluationInconclusive("diagnostic evidence is incomplete")
score = sink.evaluate(result)
```

## 5. Server、Registry 与发现

### 5.1 Server 边界

Diagnostic server 在一个 origin 下同时提供：

- Troupe 自有的静态 Web UI。
- 版本化 identity、snapshot 和 query API。
- 单向 live event stream。
- CLI 所需的查询与一致性 dump endpoint。

完整 Production Runtime 总是启动 server，不提供 `off` 或 best-effort mode。逻辑默认配置为
`bind_host="0.0.0.0"`、`port=0`：由 OS 原子分配空闲端口，不扫描端口范围。调用方可以显式覆盖
bind host 和 port；值无效或显式端口不可绑定时在 import Production 前启动失败。V1 Runtime CLI
configuration 固定为：

```console
troupe --production /path/to/my_production \
  --diagnostic-bind-host 0.0.0.0 \
  --diagnostic-port 0 \
  --diagnostic-advertise-url http://troupe-host.local:43120 \
  --diagnostic-max-run-bytes 10GiB \
  --diagnostic-writer-stall-timeout 10s \
  --diagnostic-shutdown-timeout 30s \
  -- <production-args>
```

这些 flags 都属于 Troupe，必须出现在把其余 token 原样交给 Production 的第一个 `--` separator 之前。
`--diagnostic-max-run-bytes` 缺省 unset；byte size 接受 canonical IEC unit（`KiB/MiB/GiB/TiB`）或无
suffix decimal bytes，必须是正整数。两个 duration 接受正数加 `ms/s/m/h`，默认分别是 10 s 和 30 s。
V1 不定义对应 environment variable 或 Python configuration surface，也不存在 diagnostic `enabled`、
`registry_root`、authentication、queue/batch tuning 或 automatic-retention flag。

V1 server 与 Runtime 在同一个 OS 进程中，由 Runtime supervisor 持有；它不是需要用户另行启动的
daemon，也不是独立子进程。server 使用与 Production asyncio loop 隔离的受监督 execution context
处理 listener 和 HTTP I/O。execution context 退出、listener 意外关闭或 server 无法继续提供服务时，
supervisor 将其视为 diagnostic core failure，并触发第 3 节的 Production 终止流程。客户端断开、
单个请求失败、无效请求或慢客户端只影响对应 client，不构成 server failure。

`0.0.0.0` 只是 bind address，不是客户端 URL。Registry 因此不能只保存一个含义模糊的
`address`，至少要区分：

- 实际 bind address 与 port。
- 本机 CLI 可替换为 loopback 的 local endpoint。
- 可选的 `advertise_url`，供远端机器使用 DNS 名称、主机 IP 或反向代理入口。

### 5.2 Registry

Registry 位于固定的 `<production-root>/.troupe/` 下。`production-root` 是入口解析得到的 Production
package root；不允许通过配置把 state/registry root 指向其他位置。V1 固定目录形状为：

```text
.troupe/
└── diagnostics/
    ├── instances/
    │   └── <run-id>.json
    └── runs/
        └── <run-id>/
            └── diagnostics.sqlite3
```

`instances/` 是当前 diagnostic server 的发现层：每个完成启动的 Run 拥有一个
`instances/<run-id>.json`。`runs/` 是持久化 Run 数据的独立 namespace；每个 Run 使用第 9.1 节定义的
独立 SQLite store，active 期间还可能存在 SQLite 管理的 `-wal`/`-shm` sidecar。删除 instance entry
绝不隐式删除对应 Run 数据。

不使用会被并发 Run 互相覆盖的 singleton `active.json`，也不维护隐式 `latest` symlink/file。扫描可以
同时返回零个、一个或多个 entry；存在多个 active Run 时，CLI/API 必须要求调用方显式选择 `run_id`，
不能按 mtime、文件名字典序或“最近启动”静默猜测。第 7.1 节定义 exact selection UX。

Instance entry 是发布后不再修改的静态 locator，不是运行状态数据库。V1 logical fields 至少包含：

- `registry_schema_version` 与 `server_protocol_version`。
- canonical `run_id`。
- owner `pid` 和可抵抗 PID reuse 的 `process_identity`。
- `bind_host`、实际 `port` 和本机可连接的 `local_endpoint`。
- literal `security_scope="trusted_network"` 和可选 `advertise_url`。
- wall-clock `started_at`。

`process_identity` 的 exact platform encoding 留给实现 contract，但必须能区分“同一 PID 的原进程仍在”
与“PID 已被另一进程复用”，不能只保存 PID。entry 不保存 `running/stopped/failed` 等动态 Production
状态；连接成功后，客户端从 server identity/status endpoint 查询，并校验其 `run_id` 与 entry 完全一致。

Runtime 在 import 或执行任何 Production 用户代码前创建 `.troupe/`，并用实际的 create/write/close/
remove probe 验证当前进程可写；只检查 permission bit 不足以满足该前置条件。如果 path 被普通文件占用、
无法创建目录或 probe 失败，Troupe 直接启动失败。不会回退到 `/tmp`、用户 home 或其他 state root。
运行中后续 registry/store 写入仍可能因磁盘满、权限变化或 I/O error 失败，并按 core failure 终止
Production。

Troupe创建和拥有的`.troupe/diagnostics`、`instances`、`runs`与每个Run directory固定为owner-only
`0700`；instance locator、temporary locator、SQLite database及其WAL/SHM sidecar固定为owner-only`0600`。
实现不能依赖进程umask：即使`umask 000`也必须在文件可见/使用前设置并以`fstat`复核exact mode。遇到
既有更宽mode时只能安全收紧，否则启动或运行中的持久化操作失败；chmod/fchmod/fstat failure按core
failure处理。这些本机权限不是网络认证，不能改变trusted-LAN边界。

store 和 listener 全部 ready 后，Runtime 在 `instances/` 内创建 exclusive temporary file，写入
完整 entry 并 `fsync` file，然后 same-directory atomic rename 为 `<run-id>.json`，再对 `instances/`
directory 执行 supported-platform durable sync。目标已存在、flush、rename 或 directory sync 失败均使
启动失败；不得覆盖另一个 entry。只有上述步骤全部完成才允许 import/构造 Production。Registry ready
后、import Production 前，Runtime 向 stderr 输出恰好一行 ready locator；stdout 保留给 Production：

```text
troupe: diagnostic ready {"locator_schema_version":1,"run_id":"...","local_url":"http://127.0.0.1:43120","advertise_url":null,"archive_directory":"/abs/prod/.troupe/diagnostics/runs/...","security_scope":"trusted_network"}
```

稳定前缀后必须是单行 UTF-8 JSON，不使用空格分隔的 ad-hoc fields；路径与 URL 因而可以无损包含空格。
`run_id` 不截断，`archive_directory` 是 absolute normalized path，`advertise_url` 未配置时显式为 `null`。
这行输出不是 canonical event，也不进入 Production stdout；如果 registry 未 ready 就不得输出。

正常 shutdown 时，server 按第 9.1 节完成 terminal event/final metadata commit 并向现有 subscriber
发送 `stream_closed`，然后 unlink instance entry、durably sync `instances/` directory，最后关闭
listener/store。它不在 Production 结束后继续 daemonize。因此 entry 存在的含义是“owner 宣称这个
diagnostic server 已 ready 且仍可被发现”，而不是“Production 当前仍在执行业务”。进程崩溃可能留下
stale entry，所以 entry 的存在从来不是无需校验的存活证明；entry 消失也不删除 completed/incomplete
Run archive。

CLI/server discovery 对每个候选 entry 使用以下 closed classification：

| Classification | 判定与行为 |
|---|---|
| `active` | owner process identity 匹配，endpoint 可达，server identity 的 `run_id` 匹配；允许连接 |
| `definite_stale` | owner process 不存在，或 PID 存在但 `process_identity` 不匹配；允许自动删除 entry |
| `unhealthy` | owner process identity 匹配，但 endpoint 当前不可达；报告但不自动删除，Runtime supervisor 应正在终止或服务可能处于瞬时状态 |
| `identity_mismatch` | endpoint 可达但返回不同 `run_id`；绝不连接，报告 unsafe/stale，但不由 discovery client 自动删除 |
| `invalid` | JSON、required field、filename/run ID 或 value validation 失败；报告并保留，以便人工检查 |
| `incompatible` | `registry_schema_version` 高于客户端支持范围；报告并保留，旧客户端不得删除新版本 entry |

自动清理只针对 `definite_stale`。清理前必须针对同一路径重新读取并确认 entry 内容/identity 与判定时
一致；内容已经变化时放弃本次删除。`run_id` 不复用，因此正常 Runtime 不会替换同名 entry。
`unhealthy` 不因固定时间阈值自动升级为 stale；只要 owner identity 仍匹配，就不能从 endpoint 暂时
不可达推导出进程已经死亡。损坏、版本过新和 identity mismatch 同样保守保留，避免旧客户端或错误
endpoint 破坏仍可能有价值的状态。

### 5.3 远端访问安全

V1 的 supported deployment boundary 是 trusted LAN。Diagnostic server 不实现认证、授权、login/session、
capability token 或其他 credential；任何能够连接 listener 的 network peer 都能读取 user-visible agent
message、timeline、usage 和其他已采集数据。Troupe 必须在启动输出、registry/server identity 和 Web UI
中如实标识 `security_scope="trusted_network"`，不能暗示这些 endpoint 受访问控制保护。所有 endpoint
保持只读，不提供暂停、取消或修改 Production 的控制 API。

UI 静态资源、HTTP API 和 live stream 由同一 origin 提供，浏览器代码只使用相对 URL。Server 不发送
`Access-Control-Allow-Origin` 等 CORS opt-in header，也不提供可配置 wildcard/origin allowlist。这样
不会阻止局域网 peer、CLI 或 `curl` 直接访问；它只是不授权另一个 origin 的网页 JavaScript 通过普通
浏览器 CORS 机制读取响应。V1 live transport 已固定为普通 HTTP SSE，不存在额外的 WebSocket
handshake `Origin` contract。

V1 直接 listener 使用 plain HTTP，不实现 TLS。跨不可信网络直接暴露 listener 不属于 supported
deployment；需要由部署方使用 VPN、SSH tunnel 或 TLS-terminating reverse proxy 提供网络保护。反向
代理必须让 UI、API 和 live endpoint 对浏览器保持同源；Troupe 不把 proxy authentication 当作自身
protocol contract。

`advertise_url` 是可选的显式 absolute HTTP(S) base URL，例如局域网的
`http://troupe-host.local:43120` 或反向代理的 `https://diagnostics.example/troupe`。它只写入 registry 与
server identity，用于向用户/客户端展示可连接入口，不改变 `bind_host/port`，也不影响 server identity
校验。Troupe 不枚举网卡来猜测 host IP 或 DNS name。未配置时 registry 只提供基于实际 port 的
local endpoint：wildcard bind 使用 `http://127.0.0.1:<port>`，显式 non-wildcard bind 则使用对应的本机
可连接地址。远端 CLI/浏览器必须由用户显式提供可达 URL。

`--diagnostic-advertise-url` 必须是没有 query/fragment/userinfo 的 absolute HTTP(S) base URL。Host
必填；path 缺失时规范化为 `/`，存在 path 时保留 normalized base path，并由 UI/API/SSE route 全部
相对该 base path 生成。Troupe无条件忽略任意大小写和重复组合的`Forwarded`、`X-Forwarded-Host`、
`X-Forwarded-Proto`与`X-Forwarded-Prefix`，不允许这些header改变identity、public URL、base path或route；
reverse proxy入口必须由该flag显式声明。Registry与本地持久化文件仍使用上述owner-only permission，
防止同一主机上的意外读取，但这不是网络认证机制。

## 6. 实时查询与 UI

### 6.1 增量一致性

V1 live transport 固定为 Server-Sent Events（SSE）+ UTF-8 JSON。它只承载 server-to-client 增量；
identity、snapshot、filter/query 和 dump 继续使用普通 HTTP request/response。V1 不提供 WebSocket
endpoint，也不建立双向 live command channel。

Perfetto dump的唯一live/archive HTTP route是identity-checked read-only
`GET /api/v1/dump[?through=SEQ]`。Active profile在request admission复用Runtime已持有的active exclusive
guard/capability并打开独立read transaction，绝不再次取得shared lease；archive profile先取得并在request
结束时释放shared archive lease。两者都由reader transaction捕获committed head `W`，默认
流式编码`1..W`，显式`through`必须canonical且不大于`W`；response metadata与trace内Run/W/schema/
content warning一致。Server在提交successful streaming response前必须完成T03 structural preflight；
preflight failure返回closed error且不写response body，之后的第二遍source/encode/write/disconnect failure
可以终止已经开始的stream。Request不能提供server output path、force或任何filesystem target，client disconnect
只取消本request并仅释放request-owned archive lease/reader，不释放Runtime active guard。CLI的`--url`/resolved active dump使用该route并在调用方机器执行第7.2节的atomic file publish；
local/archive CLI则对同一个captured-source/encoder core直接写本地文件。

客户端首先取得 committed snapshot：

```http
GET /api/v1/snapshot
```

```json
{
  "api_schema_version": 1,
  "run_id": "019fc634-47af-7612-9615-a974b012bbb3",
  "watermark_sequence": "1042",
  "earliest_available_sequence": "1",
  "state": {}
}
```

`state` 是从持久化事实派生、逻辑上包含所有 sequence `<= watermark_sequence` 的一致 read model；不能
读取到比 watermark 更新的半状态。`watermark_sequence` 是该 snapshot 已包含的最高 committed event。
尚无 event 时它为 `"0"`；0 只是合法 resume cursor，不是 canonical event sequence。
`earliest_available_sequence` 是当前仍可 replay 的最早 sequence，无 event 时为 `null`。

Active UI取得snapshot水位`W`后，先用同一finite events endpoint取得一个固定有界suffix：

```http
GET /api/v1/events?after=A&through=W
```

其中`A=max(0,W-4096)`。`after+through`表示在一个captured transaction内读取精确`(A,W]`，response仍是
既有`api_schema_version/run_id/captured_watermark/events/next_after`形状，`next_after`固定为`null`；不增加
`limit`参数或新API版本。`through`单独出现非法，`tail+through`与`after+tail`冲突；既有`after`和`tail`
语义不变。客户端必须验证Run identity、captured watermark不小于`W`，以及events严格、dense且精确覆盖
`(A,W]`，再把snapshot与suffix原子交给browser read-model。

取得 snapshot 水位 `W` 后，客户端订阅严格位于 `W` 之后的事件：

```http
GET /api/v1/events?after=1042
Accept: text/event-stream
```

`after` 是初次连接必填的 canonical decimal `u64` cursor。浏览器 `EventSource` 自动重连时会保持原 URL，
但发送最后处理的 SSE ID 作为 `Last-Event-ID` header；因此合法、非空的 `Last-Event-ID` 存在时必须优先
于 query `after`。两者都不存在、格式非 canonical decimal `u64` 或 cursor 大于 server committed head
时，server 不猜测位置：无有效 SSE stream 已建立时返回版本化 HTTP client error；已经进入 stream 的
cursor inconsistency 使用 `resync_required` 后关闭。

Server 先判断 effective cursor 是否仍可从当前 store 恢复。不可恢复时第一帧就是
`resync_required`，随后关闭；可恢复时捕获 committed head `H`，以 `stream_ready` 作为第一帧告知
effective cursor 和 `replay_through=H`，然后从 store 依 sequence replay `(cursor, H]`，最后无缝进入
`H` 后的新 committed event tail。Replay 期间发生的新 commit 不能落在 replay/tail 交界之外。一个连接内
`diagnostic_event` 的 sequence 严格递增且不重复；跨断线重连是 at-least-once delivery，客户端必须按
`(run_id, sequence)` 幂等去重。

```text
event: stream_ready
data: {"control_schema_version":1,"run_id":"...","resume_after":"1042","replay_through":"1050"}

event: diagnostic_event
id: 1043
data: {"schema_version":1,"run_id":"...","sequence":"1043",...}
```

只有按照第 9.1 节 SQLite `COMMIT`/`synchronous=FULL` durability boundary 已提交的 event 才能进入
snapshot、replay 或 live tail。Live UI 因而不会展示无法从 store 重放的 speculative event；commit
前只存在于 mandatory writer queue 的尾部不属于 committed head。

SSE event name 固定为下面的 closed set：

| SSE event | `id` | 语义 |
|---|---|---|
| `diagnostic_event` | canonical decimal `sequence` | 恰好一个 serialized `DiagnosticEvent`；推进 resume cursor |
| `stream_ready` | 无 | 可恢复 stream 的第一帧；声明 effective `resume_after` 和建立连接时的 `replay_through` |
| `heartbeat` | 无 | 空闲期证明 stream 仍活跃，可携带当前 committed head，但不表示该 head 已 delivery |
| `delivery_gap` | 无 | 当前 subscriber buffer 已无法完整交付；尽力说明 reason/last delivered/head，随后 server 关闭连接 |
| `resync_required` | 无 | cursor 所需事件当前不可恢复或 cursor 不再可解释；携带 current watermark/earliest available，随后关闭 |
| `stream_closed` | 无 | server 有意终止 live service；携带 reason/final committed watermark，随后关闭 |

Control frame 描述客户端如何取得事实，不是 Production 中发生的事实；它们不占用 Run sequence、不进入
event store、不发送给 `DiagnosticSink`、不导出到 Perfetto，也绝不能设置空字符串或 synthetic SSE
`id`。浏览器收到 `stream_closed` 后必须显式关闭 `EventSource`，避免自动重连。若 control frame 在网络
断开前未送达，客户端仍按最后一个 canonical SSE ID 重连，不把 control 当作已交付事实。

每个 SSE subscriber 使用独立有界 buffer，绝不对 mandatory writer 或 Production 施加背压。Buffer
overflow 时 server 不允许静默丢弃若干 canonical event 后在同一连接继续；它清除/终止该 delivery，
尽力发送无 ID 的 `delivery_gap`，随后断开。只要对应 Run store 存在，重连就从持久化 store 补齐；V1
不裁剪 active 或 retained archive 内的前缀，所以 existing non-empty Run 的
`earliest_available_sequence` 固定为 `"1"`，空 Run 仍为 `null`。整个 Run archive 已按 retention policy
删除时不存在可连接的 archive server，而不是留下一个
截断 store。`resync_required` 仍用于 cursor 无法解释及后续兼容 policy，不能把 archive-not-found
伪装成截断历史。heartbeat interval、subscriber buffer byte/event limits 和 reconnect delay 都必须
有限并由 server identity/status 报告，但属于可调 operational defaults，不是 wire compatibility 常量。

SSE response 使用 `Content-Type: text/event-stream; charset=utf-8` 和
`Cache-Control: no-cache, no-transform`，每个完整 frame 必须及时 flush；部署反向代理时必须关闭该路径
的 response buffering。一个 `diagnostic_event` frame 只含一个 event JSON object，不能把多个 canonical
event 塞入一个 batch frame，避免单帧损坏扩大到多个 cursor。

JSON wire 使用 snake_case。所有在 schema 中声明为 `u64` 的值都编码为无前导零的十进制 string，包括
sequence、elapsed time、span/causal reference、byte/count/context value；零编码为 `"0"`。D39 的
non-negative token `int` 及其 aggregate 同样编码为 canonical decimal string，以便 JavaScript client
无损读取；这是 wire 精确性约定，不把这些 public token 字段重新声明为 `u64`。
`schema_version` 等有界 version literal 仍是 JSON number，UUID 使用 canonical lowercase string，
`DecimalString` 保持已定义的规范 decimal string。每个 union variant 只携带其声明字段，但该 variant
声明的 optional 字段缺失时显式编码为 `null`。客户端不能把这些 decimal string 转成 JavaScript
`number` 后再用于 identity、排序或 resume cursor。

Instrumentation 在 hub 接受前丢失 observation 时仍发布 canonical `ObservationGap`；SSE subscriber
delivery failure 只使用上述 control/重连语义，不能从 sequence 跳跃猜测原因，也不能伪造一个全局
`ObservationGap`。Web UI 从 canonical events 派生出的 timeline row、聚合值和告警视图同样是 read
model，不会反向成为新的 Runtime event。

### 6.2 实时页面

实时页面以现有交互原型为视觉和交互基线，但原型中的 mock event shape 不是协议。第一版工作台
至少包含：

- 可缩放、平移、follow-now 的层级 timeline。
- Production/Scene/Actor/Cue/Act/tool 的 open 与 completed span。
- 选中 Act 的实时 Agent output、live events 表格、事件 inspector、usage/counter 视图和 Python
  custom views。
- Actor、事件类别、错误等过滤。
- “暂停视图”只冻结浏览器呈现，Runtime 与客户端 cursor/watermark ingestion 继续；UI 显示尚未呈现的
  sequence 数量。浏览器只保留有界 hot window，恢复时对已淘汰的范围重新 query，再追到最新状态。

对于同一 Actor 的多个 Cue，固定展示语义是：

```text
Scene 0042
└── Actor investigator        1 done / 1 running / 1 queued
    ├── Cue c-102             completed
    │   ├── mailbox wait
    │   ├── Actor.cued()
    │   └── Act #1 / tools / result
    ├── Cue c-103             running
    │   └── ...
    └── Cue c-104             queued
        └── mailbox wait ─────────── now
```

Actor 行只做聚合和定位；不能把多个 Cue 的 Act 合并到同一条无 Cue identity 的轨道。不同 Actor
可以并发，同一个 Actor 的 mailbox serialization 通过独立 wait/execution span 表达。Cue 折叠时
只隐藏其子行，不能隐藏 Cue 自身的排队、执行和 outcome。

Timeline 与 agent output 使用 master-detail 联动：timeline 回答“哪个 Actor 在何时执行哪个
Cue/Act”，选中 Act 后的下方面板回答“agent 正在输出什么、调用什么 tool、result 为何被拒绝或
接受”。消息、tool、result 和 usage 都携带相同的 `scene_id/actor_id/cue_id/act_id`；点击消息或
tool 可以反向高亮 timeline 时间点。没有选择 Act 时，UI 可以显示所有活跃 Actor 的摘要流，但
完整 transcript 必须保持按 Actor/Cue/Act 分组，不能把并发 agent 文本拼成一段。

Agent output 至少提供 `Messages / Tools / Result / Usage / Events` 视图。`AgentMessageDelta` 实时
追加到 stable message ID，tool 与 result event 以内联块插入同一 sequence；thinking 只显示状态与
duration，不显示 `AgentThoughtChunk` content。

`Usage` 视图分为 `Live context` 与 `Final Act accounting`。前者在 Act 运行中显示 session 当前
`context_used_tokens/context_window_tokens`；后者在 accounting 终结前显示 pending，随后展示
provider total/input/output 和实际提供的 thought/cache breakdown，或明确显示 partial/unavailable。
UI 显示 source；不能用 `0 tokens`、空白或 context occupancy delta 代替 unavailable。Run/Scene/Actor
汇总同时显示 token sum 和对应字段的 reported/finalized Act coverage。

### 6.3 Web UI 实现、状态与发布

Production UI 使用 strict TypeScript、Preact、`@preact/signals`、Vite、tree-shaken
`lucide-preact` 和 uPlot。Frontend source 位于 repository 中独立目录，runtime dependencies 由
`package-lock.json` exact pin；maintainer/CI 使用的 Node major 同样固定。样式使用 repository-owned
modular CSS、CSS custom properties 和 system fonts。上述依赖全是 Troupe build implementation detail，
不进入 HTTP、ViewSpec 或 Python public contract。

V1 明确不引入 React compatibility mode、client router、Redux/query framework、CSS framework、component
kit、D3/ECharts、SSR 或 runtime template compiler。UI 不从 CDN、external font 或其他第三方 origin
加载资源。所有 user/Production 内容只按 text 或受控 typed property 渲染；禁止
`dangerouslySetInnerHTML`、任意 HTML/Markdown renderer 和 inline/third-party script。

#### 6.3.1 Rendering ownership 与可访问性

Preact/Signals 负责 application shell、connection state、filter、执行树 label、inspector、agent
transcript、paginated table、usage 和四种 ViewSpec panel。Wire decode、canonical identity、cursor、query
state 与 read-model reducer 位于 framework-independent TypeScript module；Preact state 不是第二份事实源。

主层级 trace 使用一个 imperative Canvas2D renderer。它接收 immutable visible-window model，不为每个
span/event 创建 virtual-DOM node；uPlot 只渲染 server 已按共同 bucket 对齐的 `TimeSeriesView` columnar
series，不能解释 irregular span hierarchy，也不能在浏览器中重做 server aggregation。两者都不是
ViewSpec renderer extension API。

同步的 DOM execution tree 与 Canvas track 按垂直 viewport 虚拟化；Canvas 只绘制可见 row 和可见时间
范围，按 `devicePixelRatio` 分配 backing store，并把同一 turn 的 model、viewport、resize 与 hover
变化合并为每个 `requestAnimationFrame` 最多一次 draw。Pointer hit testing 使用 row-local interval index，
不能每次 move 扫描整个 Run。Exact virtualization/cache 数值是有界、受测试的 UI release constant，不是
用户可调 Runtime flag 或 wire compatibility 字段。

Canvas 不能成为唯一 semantic surface。与轨道同步的 keyboard-operable ARIA treegrid 必须暴露
Production/Scene/Actor/Cue/Act/tool hierarchy、expand/collapse、状态与 selection；选中 slice/event 的完整
文本语义由 inspector 提供。Keyboard、screen reader 与 pointer 必须共享同一个 selection model。Desktop
是 dense trace 的主要表面；small/touch viewport 仍须保留导航、pan/zoom、selection 和可读 detail，不能
通过重叠或裁掉内容来假装支持。

#### 6.3.2 有界 browser read model

浏览器只持有 visible query window、selected/expanded detail，以及固定上限的 adjacent-window LRU。
Native `fetch` 用于 bootstrap/snapshot/query，native `EventSource` 用于 committed live tail；SSE 推进
watermark、更新 live-edge projection 并 invalidate 受影响 query。页面不能把 active Run 从 sequence 1
开始的全部 canonical history 镜像进内存。

Bootstrap/resync hydration中，snapshot是spans/messages/plans/counters/usage/gaps等materialized事实的唯一
authority；bounded suffix只填充raw EventTable，并补snapshot没有表达的`tool.updated`与result-validation
instant transitions。Tool start/finish仍以snapshot span为准并可由suffix按sequence补更新。`A>0`时相应
tool/result projection显式标记需要server refresh及`dropped_through=A`。禁止把suffix中的`<=W`事件逐条送入
普通`event_received`路径，因为那会重复snapshot已经materialize的message/counter/gap事实。

Pause 只冻结 presentation。客户端继续记录最新 committed watermark 和 unseen sequence count，并可更新
有界 live-edge projection；hot data 被淘汰后，resume 通过 captured-range query 恢复，而不是依赖无界
raw-event backlog。Reconnect、gap 和 resync 同样回到第 6.1 节的 server-backed snapshot/replay contract，
不能由 UI 私有 cache 猜测缺失事实。

Schema-declared `u64` identity、watermark、cursor 和 elapsed time 始终保持 canonical decimal string 或
`bigint`；只有先减去 viewport origin 后、已验证在有界可表示范围内的 elapsed delta 才能转换为
JavaScript `number` 计算 pixel。Page state 仅存于当前内存和非内容型 URL navigation state；V1 不使用
IndexedDB、localStorage diagnostic cache 或 service worker。页面关闭或 reload 后释放 diagnostic content，
再从 server 重建。

V1 也不使用 Web Worker 或 WebGL。只有 pinned-browser profile 证明在已冻结有界窗口下 decode/layout
无法满足 main-thread performance baseline，后续版本才可以通过独立决策引入 worker；不能为了预想的
性能问题先增加第二套 message/state lifecycle。

#### 6.3.3 Deterministic build 与 wheel embedding

Repository 提供一个 maintainer frontend build entrypoint。它执行 pinned `npm ci`、strict type/unit/browser
checks 和唯一 deterministic Vite production build；build 固定 relative base、explicit ES2020 target、无
dynamic chunk、一个 JavaScript entry、一个 CSS entry，且不发布 source map。Release CI 可以把 source
map 保存为 private artifact，但 server 与 wheel 都不能携带或提供它。

Build 产出 raw、Brotli 和 gzip representations、asset manifest、generated Rust include table 与
third-party notices；这些 generated artifacts checked in。CI 在 clean environment 重新生成并要求 byte 与
hash 完全一致。普通 maturin/sdist/wheel build 只消费 checked-in artifacts，不调用 Node/npm、不访问
network，`pyproject.toml` build-system requirement 仍只包含 maturin。

Rust native module 通过 generated `include_bytes!`/static byte slice 嵌入 UI，保持 wheel inventory 为现有
Python wrapper/stub 加单一 native module，不新增 loose static-asset package tree。UI bytes 不复制到
`.troupe/`、active Run store 或 completed archive；active server 与 `diagnostic serve` 从自身 Troupe build
提供相同 embedded bundle。因此 archive 是 canonical diagnostics，不是创建它的旧 UI executable snapshot。

Asset URL 包含完整 content build hash，并相对 HTML document 解析，以便显式 reverse-proxy subpath。
Server 提供 exact MIME、`HEAD`、conditional request、representation-specific strong ETag，并按
`Accept-Encoding` 选择预生成 Brotli/gzip；Runtime event loop 不动态压缩。HTTP policy 固定为：

| Resource | Required cache policy |
|---|---|
| HTML shell | `Cache-Control: no-cache` + strong ETag |
| Content-hashed JavaScript/CSS | `Cache-Control: public, max-age=31536000, immutable` |
| Bootstrap/API/query | `Cache-Control: no-store` |
| SSE | `Cache-Control: no-cache, no-transform` |
| Any negotiated representation | `Vary: Accept-Encoding` + representation-specific ETag |

Bootstrap 在打开 live transport 前返回并验证 UI build、HTTP API、event schema 与 ViewSpec schema
compatibility。Major incompatibility 必须进入明确的 static compatibility state，不能先消费部分 event 再
以 renderer error 形式失败。

HTML 与 asset response 固定至少携带：

```text
Content-Security-Policy: default-src 'none'; script-src 'self'; style-src-elem 'self'; style-src-attr 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'; object-src 'none'; worker-src 'none'
X-Content-Type-Options: nosniff
Referrer-Policy: no-referrer
Cross-Origin-Resource-Policy: same-origin
```

`style-src-attr 'unsafe-inline'` 只允许 Preact/uPlot 的几何 layout style attribute；它不允许 inline script、
user HTML 或 external style。D21 的 no-CORS same-origin contract 保持不变。

V1 browser floor 是 Chromium/Edge 111、Firefox 115、Safari 16.4 及相应 mobile engine，不生成
legacy/polyfill bundle。缺少 required platform capability 时只呈现不启动 query/live transport 的 static
compatibility state；不能留下一个看似工作但丢失事实的 dashboard。

#### 6.3.4 Release budgets 与验证

以下是不可由用户调大的 release gate：

| Artifact budget | Maximum |
|---|---:|
| Logical uncompressed HTML + JavaScript + CSS | 512 KiB |
| First-load HTML + JavaScript + CSS Brotli transfer | 160 KiB |
| All embedded raw/gzip/Brotli UI representations + notices | 768 KiB |

Frontend verification 至少包括 strict TypeScript 与 protocol reducer/decimal unit tests、Rust-to-browser
shared canonical fixtures、Playwright Chromium/Firefox/WebKit interaction tests、desktop/mobile screenshot、
Canvas nonblank/pixel check、keyboard/ARIA/axe、malformed/XSS content、reconnect/gap/resync/pause、cache/CSP/
compression、clean deterministic rebuild byte comparison，以及从最终 wheel 启动 active/archive UI 的 smoke
test。

另有 pinned Chromium stress fixture：long Run、10,000 个 visible primitives 和持续 live updates。它必须
证明 browser window/LRU/heap 有界、canonical read-model state 不丢失、Canvas 每 animation frame 最多 draw
一次，并通过 repository 中显式 checked-in performance baseline threshold。性能门限只能按有记录的
benchmark review 更新，不能由测试运行时自动重写基线。

## 7. CLI 与 Perfetto 导出

### 7.1 CLI 能力

Active Run 上，`troupe diagnostic` 是 diagnostic server 的 CLI client。Completed/incomplete local archive
上，CLI 复用同一个 versioned read-only query/server implementation；不能另造一套 event pairing、usage
aggregation 或 snapshot 解释逻辑。`diagnostic` 是真正的 Troupe top-level command branch；现有
`troupe --production ... -- <production-args>` run syntax 保持兼容，不强制迁移成 `troupe run`。

所有 diagnostic command 只做 filesystem/server protocol 操作，绝不 import package、执行
`__init__.py/production.py` 或构造 `Production`。`--production` 在这些命令中只表示包含 `.troupe` 的
Production root directory；即使业务代码已经损坏、移动或无法 import，只要 state directory 仍可读取，
archive 就必须仍可诊断。

V1 exact command surface 为：

```console
troupe diagnostic runs --production PROD [--format human|json]

troupe diagnostic status TARGET [--format human|json]
troupe diagnostic snapshot TARGET [--format human|json]
troupe diagnostic events TARGET [--tail N | --after SEQ] [--follow] \
  [--format human|jsonl]
troupe diagnostic dump TARGET --output FILE [--through SEQ] [--force]

troupe diagnostic serve \
  (--production PROD --run RUN_ID | --archive RUN_DIRECTORY) \
  [--port PORT] [--open]

troupe diagnostic cleanup --production PROD \
  (--run RUN_ID | --older-than DURATION | --keep-runs N | --max-total-bytes SIZE) \
  [--apply] [--format human|json]
```

上面的 `TARGET` 不是 literal token，而是下面三组 mutually exclusive selector 中恰好一组：

```console
--production PROD [--run RUN_ID]
--url BASE_URL
--archive RUN_DIRECTORY
```

`--run` 只可与 `--production` 同用，值是 canonical lowercase UUID。`--url` 必须是没有
query/fragment/userinfo 的 absolute HTTP(S) base URL，并通过 identity/protocol compatibility check；它
不从本机 registry 猜 expected Run。`--archive` 指包含 `diagnostics.sqlite3` 及可能 WAL sidecar 的完整
Run directory，内部 `run_id` 是权威 identity；目录可以是脱离原 Production root 的只读 copy，因此不
要求 basename 等于 UUID，但不接受裸 database file。Selector 缺失、混用或不适用于 subcommand 是
usage error，不回退猜测另一个 target。

`runs` 只接受 `--production`，合并展示 registry candidate 与 archive metadata，包括 `active`、
`definite_stale`、`unhealthy`、`identity_mismatch`、`invalid`、`incompatible`、`completed` 和 `incomplete`；
无法取得可信 Run ID 的 entry 仍按 path 单独列出。Human output 始终显示完整 `run_id`，没有 candidate 时
返回成功的空结果。

#### Local target resolution

`--production PROD` 的 resolver 扫描并验证第 5.2 节 registry 与 `runs/`，但不把任意 archive 的存在
解释成 live owner 已消失：

- 显式 `--run R` 时，存在 identity-validated `active` entry 就连接其 `local_endpoint`；不存在对应
  instance entry 时才打开同 ID archive。
- 对应 entry 是 revalidated `definite_stale` 时，可以按 D20 durable unpublish 后打开 archive，并把
  `clean_shutdown=false` 如实呈现。对应 entry 是 `unhealthy`、`identity_mismatch`、`invalid` 或
  `incompatible` 时返回 discovery failure，不能绕过 owner/compatibility check 直接读取 SQLite。
- 没有 `--run` 时，只有一个可安全确认的 `active` Run 且不存在其他 potentially-live ambiguous entry，
  就选择该 Run；任意数量的历史 archive 不使它歧义。多个 active 或存在无法排除的 unhealthy/invalid/
  incompatible/identity-mismatch candidate 时要求调用方先用 `runs` 检查并显式选择，不能猜测。
- 没有 active/potentially-live candidate 时，恰好一个 valid archive 才能隐式选择；零个是 not-found，
  多个要求 `--run`。任何路径都不按 mtime、`started_at`、`ended_at`、目录名字典序或业务 outcome 定义
  implicit latest。

上述回退只决定同一 query implementation 从 live HTTP 还是 local archive 取得事实，不改变 endpoint/
store 的 schema。Resolver 成功后仍校验 target 返回/保存的 `run_id` 与所选 identity 一致。

#### Query、event 与 output

`status` 返回 diagnostic infrastructure、Production outcome、committed/read-model watermark、active
writer queue/progress、effective limits/configuration、security scope 和 `clean_shutdown`；archive 中没有的
live-only field 必须是 explicit unavailable，而不是零。`snapshot` 在一个 committed watermark 上返回
Web UI 使用的 Scenes/Actors/Cues/messages/usage/counter read model。

`events` 的 `--tail` 是 non-negative event count，`--after` 是 canonical decimal `u64` cursor；二者显式
互斥。都未给出时等价于 `--tail 100`，`--after 0` 表示从完整 retained prefix 开始，
`--tail 0 --follow` 表示从连接时 captured committed head 之后开始。Finite query 捕获一个 head 并只输出
该水位内的结果；`--follow` 先无缝 replay 所选初始 range，再跟随 active committed SSE tail。Resolved
archive target 不支持 `--follow`，不能通过轮询 SQLite 伪装实时流。

CLI 对 live reconnect 实现 D23 的 `(run_id, sequence)` deduplication；无论 human 还是 JSONL，stdout 中
canonical event 的 sequence 严格递增且不重复。`stream_ready/heartbeat/delivery_gap/resync_required/
stream_closed`、连接重试和 warning 都不写入 JSONL fact stream：普通结束由 `stream_closed` 收束，暂时
断线从最后已输出 sequence 恢复，无法恢复或 identity 改变则在 stderr 报错并失败。每个 JSONL line 是
一个未经 wrapper 的 canonical `DiagnosticEvent` JSON object。

`runs/status/snapshot` 默认 `--format human`，并支持一个以 newline 结束的 versioned JSON document；
`events` 默认 human，并支持 canonical `jsonl`。Human layout、颜色和列宽不是机器兼容 contract；JSON/
JSONL 的 schema/version、完整 UUID、decimal-string `u64` 与 explicit `null` 才是稳定接口。Machine
format 的 stdout 只包含请求结果，warning/progress/error 只写 stderr。

CLI exit status 固定为：command 成功是 0；discovery ambiguity/not-found、server/protocol/store/export/
cleanup operation failure 是 1；Clap usage/argument error 是 2；用户中断是 130。成功观察到
`outcome=failed` 或 `clean_shutdown=false` 的 Run 仍是 command success 0，状态本身通过 payload/human
output 表达，不能让脚本混淆“被观察对象失败”和“观察命令失败”。

#### Archive serve 与 cleanup

`serve` 只接受显式 inactive target：`--production PROD --run R` 或 `--archive RUN_DIRECTORY`。如果该
Run 仍 active、存在非-definite-stale owner ambiguity、schema incompatible 或无法取得 shared archive
lease，命令失败；它不接受 `--url`，因为 active server 已经提供 Web UI。Archive server 固定 bind
loopback，`--port` 默认 0 由 OS 分配且不能配置 non-loopback host；它在 foreground 运行，ready 后向
stderr 输出稳定前缀 `troupe: diagnostic archive ready ` 加一行 versioned JSON locator（至少包含完整
`run_id`、实际 `local_url`、absolute archive directory 和 `clean_shutdown`），`--open` 才调用系统
browser。Browser launch 失败只在 stderr warning，已经 ready 的 server 继续前台运行。它使用同一 static
UI/query implementation，明确显示 completed/incomplete 状态，不写 event/store，不发布 `instances/`
registry；退出即释放 lease。`--port` 必须是 `0..65535` 的 decimal integer。

`cleanup` 只操作 `--production PROD` 下由 internal metadata 验证的 Run directory，默认是 preview；只有
`--apply` 才产生删除。每次必须恰好选择一个 policy：

- `--run R` 精确选择一个 inactive、unleased completed 或 incomplete archive。
- `--older-than DURATION` 选择 `ended_at` 早于 cutoff 的 cleanly finalized archive。
- `--keep-runs N` 保留按 `ended_at`、`started_at`、`run_id` deterministic ordering 最新的 N 个 cleanly
  finalized archive。
- `--max-total-bytes SIZE` 按同一 ordering 从最旧开始选择 cleanly finalized archive，直到 validated Run
  directories 的 apparent regular-file bytes 不超过 budget；受保护数据本身超过 budget 时明确报告无法
  满足 policy。

`--older-than` 接受正整数加 `h/d/w`；`--keep-runs` 接受 non-negative decimal integer；
`--max-total-bytes` 使用 Runtime quota 相同的 canonical byte-size grammar。Exact `--run` 在 preview 中可
报告 active/leased/protected reason，但 `--apply` 无法删除 exact target 时 exit 1。Batch policy 对
protected/leased candidate 逐项报告 skip；若 remaining eligible archive 足以满足 policy 可以成功，否则
（例如 protected bytes 本身超过 `--max-total-bytes`）exit 1。

Batch policy 只自动选择 `clean_shutdown=true` 的 archive，不因 business outcome failed 排除它，也不
自动删除 incomplete archive；后者必须使用 exact `--run`。Preview 列出 exact Run ID、bytes、选择原因与
protected/skipped reason。`--apply` 在每个删除前重新检查 registry identity 与 exclusive lease，跳过
active/leased/raced Run；先通过 same-filesystem atomic rename 把整个 directory 移出 `runs/` discoverable
namespace，再递归删除 regular directory content，全程不跟随 symlink。不能单独删除 event、table、WAL
或其他 archive 子文件。

### 7.2 Perfetto exporter

Perfetto exporter 从event store的确定`watermark_sequence`读取不可变snapshot，再生成原生TrackEvent
`.pftrace`。它的核心接口是bounded `CapturedEventSource -> AsyncWrite/packet stream`，由local atomic-file
wrapper和`GET /api/v1/dump`共用。`diagnostic dump`未指定`--through`时，在local command或remote request
admission捕获当前committed head `W`并导出完整prefix `1..W`；指定时必须是canonical decimal`u64`且不大于
admission时的head，然后导出`1..through`。空Run/`--through 0`生成有Run descriptor但没有event packet的
合法trace。导出期间到达的新event属于下一个水位，不能产生前后自相矛盾的半快照。

`--output` 必须是 filesystem path，V1 不接受 `-`/stdout。Exporter 在 output parent 中创建 exclusive
temporary regular file，完成 encoding、flush、file sync 和 close 后才进入 same-directory namespace
publication。目标不存在时使用 no-replace atomic rename；`--force` 只允许替换经 dirfd+lstat/identity 验证的
regular file，拒绝 directory、symlink 或其他 file type，并先以 exclusive same-directory backup hard link
保存旧 inode、durable sync parent，再 atomic replace。每次 namespace mutation 后都 sync parent directory。

Publication outcome 是 closed 三态：`published` 表示新 target 及 backup/temp 清理均 durably 完成；
`not_published` 表示任一 pre-commit failure 后已通过 identity-checked rename/unlink 与 directory sync 证明
目标仍为原 inode（或仍不存在）；`publication_indeterminate` 表示 post-rename directory sync、rollback、
backup unlink 或 rollback/cleanup sync 失败，无法证明最终 namespace。后者必须 exit 1 并报告 stable phase、
target 及已验证 identity，不得谎称旧目标保留，也不得删除 identity 不匹配的 path。成功不留下 temp/backup；
普通失败尽力 rollback，并且只有 durable 证明完成后才能报告 `not_published`。成功后 human stderr 明确报告
`run_id`、exported through watermark、event count、output path 和敏感数据提醒；`.pftrace` bytes 不写 stdout。

#### Encoder 与 schema 边界

Exporter 的唯一新增 runtime crate 是 exact-pinned `prost 0.14.4`，只在 private module 中声明 Troupe
实际写出的 stable-public protobuf message/field。Schema provenance 固定为 official Perfetto v57.2 commit
`da1d152cff27890903d158fe96751de3aab883cc`；repository 保存所需 upstream proto snapshot、license、逐文件
SHA-256，以及列出每个使用中的 message、field number/type 和 enum value 的 closed manifest。Snapshot
必须包含每个实际使用定义所在的 upstream 文件，至少包括
`protos/perfetto/common/builtin_clock.proto` 中的 `BUILTIN_CLOCK_TRACE_FILE=11` 和
`protos/perfetto/trace/track_event/debug_annotation.proto`。Maintainer schema audit 同时验证 private
declaration、snapshot 与 closed used-definition manifest：closure 只包含每个实际镜像的 message/field/enum
定义及被选字段所引用的 message/enum 定义；缺少其中任一定义、field number/type/cardinality 或 enum value
即失败。上游文件中未被选择的 import/oneof arm 明确不属于该 closure，audit 不尝试编译完整 upstream
schema，也不能把“import target 存在”冒充“使用定义已闭合”。升级必须显式 review 并重建
golden/compatibility fixture，不能在 build 中自动取得 latest schema。

普通 source、sdist、maturin 和 wheel build，以及用户执行 dump 时，都不运行 `prost-build`/`protoc`，不
下载工具或 schema，也不依赖 Perfetto SDK/FFI、Trace Processor、第三方 exporter、Node 或 public service。
Perfetto UI、Trace Processor 与 compatibility binary 只允许存在于 dedicated CI job。这个 minimal subset
用于限制 Troupe 承诺和审计的 wire surface，不以减少 wheel bytes 为产品目标。

原生`Trace`是repeated field 1的`TracePacket` stream。Exporter先完整扫描同一个captured prefix，验证dense
sequence/reference/timestamp/ID并建立D52所需的structural index；preflight成功后写descriptor/metadata
prelude，再第二次扫描同一prefix并按canonical order写event packet。每次只将一个packet编码为field-1
fragment，使用reusable scratch buffer立即写入调用方的bounded writer，不在内存构造完整event prefix或
trace。Structural index同时受固定1,000,000 entries与64 MiB owned payload限制，计入validator sequence/span
record、unique track、collected span、lane assignment、causal flow及其start/end attachment、Act usage、dense
identity和descriptor order；每次reservation前checked arithmetic，等于上限成功，下一项返回typed
`StructuralIndexLimitExceeded { dimension, limit, required }`且不得先分配。调用方不能覆盖上限，禁止
filesystem spill/temp index。Invalid through、non-dense/reference/timestamp、ID exhaustion和structural limit
必须在第一次writer poll前失败；第二遍source/encode/write/cancellation failure可以发生在partial stream后。
Local wrapper的writer是exclusive temporary file，HTTP route的writer是具备disconnect cancellation的response
body。Peak memory由fixed structural ceiling、一个source page、一个packet和reusable buffer共同约束。

所有 timestamped packet 显式设置 `timestamp_clock_id=BUILTIN_CLOCK_TRACE_FILE(11)`，timestamp 直接使用
canonical Run-relative `elapsed_ns`。Descriptor packet 无 timestamp；不生成 wall-clock、BOOTTIME/
MONOTONIC 声明或 ClockSnapshot。由于 Trace Processor 使用 signed 64-bit nanoseconds，任一 `elapsed_ns`
大于 `i64::MAX` 都使 dump 明确失败，不能 wrap、clamp、rescale 或换时钟。

V1 只写 direct、non-interned TrackEvent slice begin/end、instant、counter、fixed64 flow/
terminating-flow ID 和 bounded debug annotation。所有 TrackEvent 与 TrackDescriptor packet 显式设置固定
`trusted_packet_sequence_id=1`，这是 v57.2 ingestion 所需 identity，不代表启用 incremental state。禁止
interned data、incremental-state clear/default、incremental timestamp、compressed packet、custom proto extension、
legacy event，以及 unstable Chrome/Android field。Counter track 的 TrackDescriptor 必须携带空
`CounterDescriptor` presence marker；普通 timeline track 不携带该字段。Required
Troupe metadata 不能依赖 v57.2 中仍不稳定的 `TraceAttributes`。

#### 确定性映射与精度

映射固定如下：

| Troupe 语义 | Perfetto 表达 |
|---|---|
| Production lifecycle、Scene、Cue、`cued()`、Act caller、agent turn、tool | slice |
| Actor | logical track group，不伪装成 OS thread |
| mailbox wait | Cue 下的独立 slice，并可汇总 queue counter |
| Effect return、Cue dispatch、cancellation handoff | flow |
| context occupancy、queue depth、active turn、rejection count | counter |
| Act final token accounting | Act slice 的 namespaced annotations 和 terminal metric event；不是 context counter |
| known token aggregate 与 reported Act coverage | 两条分开的 counter |
| `ObservationGap` | 带 affected scope/time/count 的 instant marker |
| Custom span/instant/counter | 分别映射为 slice、instant 和 counter，并保留 namespaced attributes |
| outcome、stable IDs、normalized error | namespaced debug annotations |
| 仍在运行的 span | 没有 end packet 的 open slice |

Exporter 先收集 captured prefix 中的 typed track identity 和 causal-link identity，按固定 canonical order
排序，再分别分配 dense nonzero export-local `track_uuid` 与 flow ID；canonical identity 同时写入 annotation。
因此算法不依赖 hash，也不存在 collision fallback；计数超出 `u64` 表示范围则在发布前失败。所有 descriptor
先于 event，parent 先于 child，并携带明确 sibling order/no-merge 语义。Actor 是 logical group，不能伪装
为 OS process/thread。对于无法在一个 Perfetto track 上形成合法 LIFO begin/end stack 的 overlap，按
canonical span start order 选择最低可用 sibling lane；open canonical span 只写 begin，不伪造 finish。

Perfetto counter 只能承载 exact `int64` 或 finite 且可精确表示的 double。超出范围的 Python integer、任意
精度 `Decimal` 或其他不能精确投影的 numeric value，必须改为所属 scope timeline 上的 instant/annotation，
保留 canonical decimal text 并写 `counter_projection=not_exact`；instant 绝不能写到 counter track，禁止
round、clamp、silent drop 或画出虚假 counter。Unavailable/
missing usage 保持 absent 并附 availability annotation，不能解释成零。

固定的 Troupe metadata descriptor/instant 至少记录 exporter schema、canonical event schema、Run ID、
captured watermark、Troupe version、outcome/clean-shutdown availability 和 content warning。Invalid span pairing、
parent/flow reference、ID/timestamp/numeric/resource overflow、protobuf encode failure 或 output write/flush/sync/
rename failure 均遵守前述三态 atomic dump contract：command exit 1 时只能报告 durably restored
`not_published` 或明确的 `publication_indeterminate`，不能虚构旧目标不变；两者都不影响 active Production
或 canonical archive。

已清理的 Perfetto PoC 曾验证 public Perfetto 能正确读取该层级、多 Cue、open slice、counter、flow
和 annotations；该结论保留为 exporter 的可行性证据，不构成引入 Perfetto UI 源码的理由。

Troupe只把trace写到请求方指定的本地CLI output；对于`--url`/active target，server经只读dump route把
captured bytes返回给请求方，但不在server filesystem创建trace，也不自动上传、打开或把trace发送给public
Perfetto。CLI应提醒用户`.pftrace`可能包含敏感诊断信息。未来可以基于同一event snapshot增加JSONL或其他
exporter，但不能改变核心事件schema来迁就某一种输出格式。

## 8. Python 扩展模型

Python 扩展分为三层：第 4.5 节的 per-Act `DiagnosticSink` 消费内置 agent 诊断；业务插桩发布
Production-specific 语义；声明式视图定义 Web UI 如何展示已经持久化的事实。只有第一层会由
Runtime 在 Run 期间主动回调用户 Python；后两层分别是用户代码显式调用的同步 API 和启动期编译的
纯数据，HTTP request、live update 或 browser interaction 均不得执行 Production Python。

### 8.1 业务插桩 API

V1 public module 是 `troupe.diagnostics`，publication surface 精确为：

```python
from collections.abc import Mapping
from contextlib import AbstractContextManager
from decimal import Decimal
from typing import Literal, TypeAlias


DiagnosticScalar: TypeAlias = None | bool | int | float | Decimal | str
DiagnosticAttributeValue: TypeAlias = (
    DiagnosticScalar | list[DiagnosticScalar] | tuple[DiagnosticScalar, ...]
)
DiagnosticDimension: TypeAlias = bool | int | float | Decimal | str


def event(
    name: str,
    /,
    *,
    severity: Literal["debug", "info", "warning", "error"] = "info",
    attributes: Mapping[str, DiagnosticAttributeValue] | None = None,
) -> None: ...

def counter(
    name: str,
    value: int | float | Decimal,
    /,
    *,
    unit: str | None = None,
    dimensions: Mapping[str, DiagnosticDimension] | None = None,
) -> None: ...

def span(
    name: str,
    /,
    *,
    attributes: Mapping[str, DiagnosticAttributeValue] | None = None,
) -> AbstractContextManager[None]: ...
```

典型调用为：

```python
from troupe import diagnostics


with diagnostics.span(
    "orders.select_supplier",
    attributes={"region": region},
):
    ...

diagnostics.event(
    "orders.rejected",
    severity="warning",
    attributes={"reason": reason},
)

diagnostics.counter(
    "orders.pending",
    pending,
    unit="items",
    dimensions={"region": region},
)
```

`event()` 产生 `CustomInstantOccurred`。`counter()` 产生绝对 gauge sample，不表示 increment/delta；同一
series 由 `(name, unit, dimensions)` 精确标识，事件发生次数使用 event-count query 表达。V1 不提供
`increment()`、monotonic/reset 或 histogram API。

`span()` 返回的对象没有 public method；调用本身只复制并校验静态参数，进入 context 时同步 admission
`CustomSpanStarted`，退出时同步 admission matching `CustomSpanFinished`，`__enter__()` 返回 `None`。
普通退出为 `completed`，`asyncio.CancelledError` 为 `cancelled`，其他 `BaseException` 为 `failed`；V1
finish 不附加用户 terminal attributes。它是同步 context manager，因此可以包围含 `await` 的代码，
`__exit__()` 始终不抑制 body exception。没有进入 context 的对象不产生事件。

所有 Mapping/list/tuple 在相应调用或 `span()` 构造时 eager copy，不保留用户 mutable reference。
`event()`/`counter()` 返回成功和 span enter/exit 返回成功，分别表示对应 custom event 已进入 D27 的
mandatory ingress，不表示已经 SQLite commit。后续 writer、storage 或 backpressure core failure 仍按
D16/D27 终止 Production，不能因事实来自 custom instrumentation 而 drop、降级或继续。

#### Namespace、值与资源边界

Custom name 必须匹配 `^[a-z][a-z0-9_]*(?:\.[a-z][a-z0-9_]*)+$`，UTF-8/ASCII byte length 为
1..128，且第一段不能是 `troupe`。V1 不维护 namespace registry；完整 name 就是 library/Production
之间的 collision boundary。不同 producer 使用同名表示主动共享一组语义，意外 collision 由发布者
负责。

Attributes 是单层 map：value 只能是 scalar 或 scalar 的 exact `list`/`tuple`，不能包含 nested map、
nested list、bytes、任意 Python object 或 lazy iterable。Dimension value 只能是 non-`None` scalar，
不能是 list。每个 present key/unit 必须是非空 UTF-8 plain-text string；UI 始终按 text 而不是
HTML/Markdown 渲染。固定上限如下：

| 资源 | V1 上限 |
|---|---:|
| custom name | 128 bytes |
| attribute/dimension key | 64 bytes |
| counter unit | 32 bytes |
| attributes | 32 entries |
| counter dimensions | 8 entries |
| scalar list/tuple | 64 elements |
| 单个 custom variant 的 caller-supplied canonical payload | 64 KiB |

`bool` 只作为 attribute/dimension boolean；由于它是 Python `int` 的 subclass，counter value 必须显式
拒绝 `bool`。`int` 不设置脱离 payload-byte limit 的人为数值上限；所有 attribute/dimension/counter
中的 `float` 和 `Decimal` 必须 finite。Finite float 以其 shortest round-trip decimal spelling 规范化，
`Decimal` 保留精确十进制值，随后与 `int` 一起进入 canonical numeric representation；`NaN`、正负
infinity 和不可规范化值为非法。

由于 dynamic custom field 没有静态 JSON schema，canonical wire/store 不能用 bare JSON number 表示
numeric scalar。每个 dynamic scalar 携带 closed logical type tag；integer/decimal 的 value 使用
canonical decimal string，string、boolean 和 null 保持不同类型，list 的每个 element 也分别 tagged。
因此任意 Python `int`、精确 `Decimal` 和 string `"1"` 不会在 JavaScript 中混淆或丢精度。Dimension map
按 key canonical order 编码；数字经规范化后的 type/value 参与 series identity 和 equality。64-KiB 上限
在 sequence 分配前按 caller-supplied variant payload 的这份 canonical UTF-8 encoding 计算。

这些 attributes 是用户显式选择发布的业务内容。Troupe 只检查上述结构、类型和大小，不扫描
credential key、不脱敏、不改写；发布者必须按第 9.2 节的 trusted-LAN、retention 和导出边界承担内容
责任。

### 8.2 Context、identity 与 failure

Publication 只允许发生在 active Production 的 Runtime-authorized lifecycle task 或由现有 task-lineage
机制登记的后代 task 中。`start()`、Scene、`cued()`/Act 和 `stop()` 分别得到实际可证明的 Run/Scene/
Actor/Cue/Act scope；Production module import 和 constructor 不具备 publication context，构造期结构仍由
Runtime built-in instrumentation 观察。线程、未登记 task、已经关闭的 Scene/Cue/Act scope 或 Run 结束后
调用会同步抛 `DiagnosticContextError`。

Act publication authority是绑定`RunBinding + act_id/generation`的显式token，并至少区分caller、registered
caller descendant与authorized supervisor。成功Act admission在prompt submission前事务性把provisional
authority绑定到current task；任何commit失败都恢复原lineage并清理未发布authority/subscriber/registry。
Registered child只继承创建时可证明的authority；caller结束后caller descendant立即过期，只有已授权
supervisor可继续完成remote settlement。Act finish已经canonical admission且sink terminal已入队后，全局
失效该generation，再由sink settlement seal。携带过期Act token时必须同步报`DiagnosticContextError`，不能
回退到同Cue中的后续Act，也不能通过Cue scope或sink registry反查Act。

调用方不能传 timestamp、sequence、Run/Scene/Actor/Cue/Act ID、parent span ID、containing span ID 或
`caused_by`。Runtime 从当前 lineage snapshot 填充 scope，并选择当前 task 中仍 open 的最内层 custom
span，否则选择可以证明时间包含的最内层 built-in span。Custom span stack 只属于进入它的 asyncio
Task，不传播到新建 child task；已登记 child 仍继承其合法 domain scope，但不会错误地成为可能已经结束
的 custom span child。V1 API 不返回 event/span ID，business result 或控制流因而不能依赖 diagnostic
identity。

不支持的 Python type 使用 `TypeError`；非法 name/severity、非 finite number、违反结构或资源上限使用
`ValueError`；缺失或过期 lineage 使用 `DiagnosticContextError`。它们都在 sequence 分配前从显式用户
调用同步抛出，不产生 partial event，也不使 healthy diagnostic core 自身失效；用户不捕获时，像其他
Production 用户异常一样影响当前 lifecycle callback。成功 admission 后发生的 ingress/writer/server
故障使用第 10 节独立 infrastructure error surface，并把 Production 标记为 fatal，捕获调用栈上的异常
也不能恢复 Run。

进入当前 Act scope 的 `Custom*` 与所有其他消费者共用同一个 canonical event。Run store、Web UI、CLI
和 Perfetto exporter 都能观察它；per-Act `DiagnosticSink` 仅在 `capture.custom_events=True` 时接收当前
Act 的投影。`DiagnosticSink` 仍是唯一由 Runtime 主动执行的 Python diagnostic callback。

### 8.3 声明式 ViewSpec

V1 public `ViewSpec` union 固定为四个 final、frozen、slotted、keyword-only value：

```python
ViewSpec: TypeAlias = TimelineView | MetricView | TableView | TimeSeriesView
```

| Renderer | Query 结果与用途 |
|---|---|
| `TimelineView` | 筛选 custom/built-in span 与 instant，作为独立 lane 展示；open span 继续画到当前 committed/live time |
| `MetricView` | 展示一个 latest counter、event count、completed-span duration aggregate 或 Act token aggregate，同时显示 coverage |
| `TableView` | 以 cursor pagination 展示 canonical event、assembled span 或 Act usage row；列只能引用 closed stable field 或 exact custom attribute key |
| `TimeSeriesView` | 对 counter、event count、completed-span duration 或 Act token metric 做时间 bucket，展示一个或多个有界 series |

每个 view 的公共字段至少为唯一稳定 `id`、非空 plain-text `title`、对应 renderer 的 typed `query`、
`time_range: Literal["viewport", "run"]` 和 `scope: Literal["selection", "run"]`。`id` 在一个
Production 的 tuple 内唯一，匹配 `^[a-z][a-z0-9_]*$` 且不超过 64 bytes；title 不超过 128 UTF-8
bytes。`viewport` 跟随主 timeline 当前可见范围，`selection` 表示当前选择的 Run/Scene/Actor/Cue/Act
scope 及其后代；没有选择时退化为整个 Run。ViewSpec 不声明任意页面位置、CSS 或布局，具体布局属于
第 6.2、6.3 节的 Troupe-owned UI contract。

四种 renderer 各自接受 closed query descriptor，不能互换不适用的 query。Typed source 只包括：

- exact built-in `span_kind`/`instant_kind`/`counter_kind` 或 exact custom name；
- custom/built-in counter value，其中 custom counter 按每个 exact series 先选择当前/bucket 内 latest sample；
- custom instant 或 closed built-in instant 的 occurrence count；
- custom/built-in completed span 的 elapsed duration；
- `ActTokenUsageFinalized` 的六个 exact token metric 及其 availability/coverage。

Filter 只允许 exact name/kind、closed severity/outcome，以及 custom scalar attribute 的 exact equality 或
existence。Query 最多按一个 closed dimension 分组：Scene、Actor、Cue、Act、event/custom name，或一个
exact scalar attribute/dimension key。Reducer 固定为 `count`、`sum`、`min`、`max`、`mean` 和 `latest`，
且 descriptor constructor 必须拒绝 source 不支持的 reducer；`latest` 以最大 canonical sequence 决定。
Counter 在每个 exact series 内先做 latest-sample selection，再跨匹配 series reducer，不能把一段时间内
的 gauge samples 当 delta 相加。`mean` 返回 exact numerator 与 contributing count，由 renderer 格式化，
不产生无定义精度的 binary float。

`TimeSeriesView` 的 bucket 完全由 server 确定，不增加 public tuning 字段。每次 query 先冻结 Run-relative
`[range_start_ns, range_end_ns)`：`run` 从 0 到 captured watermark 对应的 captured elapsed end，`viewport`
使用 request 中与 scope/watermark 共同绑定的 exact viewport bounds；empty Run 返回零 bucket。非空 range
固定 `max_points=1024`，
`bucket_width_ns=max(1, ceil((range_end_ns-range_start_ns)/1023))`。所有 bucket 以 Run origin 0 对齐，
返回所有与 range 相交的左闭右开 `[k*width,(k+1)*width)`，因此最多 1024 个。首尾 bucket 只统计与 query
range 交集内的事实并标 `partial=true`；event count 按 event timestamp、completed-span duration 按 finish
event timestamp、Act token 按 finalized event timestamp 入 bucket，恰在右边界的事实进入下一 bucket。
Counter 在每个 bucket/series 内取最大 canonical sequence 的 sample 后再 reduce。Empty bucket 必须以 aligned
timestamp 加 `value=None, contributing_count=0` 显式返回，不能删除或前向填充；每 bucket 返回 matched/
contributing/excluded/gap/coverage，width 和 range 绑定在 response。Point cap 由 width 推导保证，不允许静默
截断；watermark、viewport 或由此得到的 width 变化使旧 response 整体 stale 并重新 query，browser/uPlot
不得自行 rebucket。

V1 query 没有 SQL、regex、range predicate、join、任意 nested field path、computed expression、Python
callable 或 user-defined aggregate。ViewSpec/attribute string 不能包含 executable HTML、Markdown、
JavaScript、CSS、external URL 或 custom renderer class；浏览器只调用 Troupe 内置 renderer，并把所有
内容字符串当 text。

所有 query 只读取 snapshot captured watermark 对应的 committed state；live 页面在新 watermark 到达后
用同一 query semantics 增量刷新。Row-producing response 使用 opaque cursor pagination，server 接受的
单页最多 500 rows；series/point 等 server operational cap 必须在 versioned capability 中声明。每个
result 都携带 `run_id`、captured watermark、time/scope binding、matched/contributing/excluded count、
相关 `ObservationGap` coverage，以及 pagination/truncation/incompatible 状态。Open span、missing/non-
numeric attribute、unavailable token 和 resource truncation 不能静默进入完整 aggregate。

### 8.4 编译、持久化与兼容

Production 可以声明版本化、可序列化的视图：

```python
class MyProduction(troupe.Production):
    diagnostic_views = (
        troupe.diagnostics.TimelineView(...),
        troupe.diagnostics.MetricView(...),
        troupe.diagnostics.TableView(...),
        troupe.diagnostics.TimeSeriesView(...),
    )
```

`diagnostic_views` 必须是 Production class 上的 exact tuple；缺失时等价于空 tuple，不接受 list、
generator、descriptor/property 或任意 user-defined ViewSpec subclass。用户 helper 可以组合并返回 built-in
frozen values，但不能提供 `compile()`、query callback 或 renderer hook。每个 value 在 Production module
import/类定义时构造并立即做局部 field validation；Production class 解析完成后，Runtime 在调用 constructor
前以 static class-attribute lookup 验证完整 tuple、ID uniqueness、query/source/reducer compatibility 和总
resource policy。无效 tuple 不允许先执行 Production constructor。

验证成功后，Runtime 在 Production constructor 前把每个 view 编译成独立的 pure JSON record；每条 record
携带 `view_schema_version`、view ID、renderer discriminator 和 query data，稳定 manifest 只索引这些
有界 records。所有 record 在 constructor 前持久化到 Run metadata。它们是 active UI 与 archive 的唯一
ViewSpec 来源；后续 query、SSE、browser interaction、`diagnostic serve` 和 archive read 均不再 import
Production，也不回调任何 Python。ViewSpec compile 发生时 diagnostic store/server 已按 D18 ready，因此
module construction 或 full-tuple compile failure 会形成可观察的 failed startup archive。

`GET /api/v1/views`无query时返回同一manifest的bounded immutable catalog，top-level固定为
`api_schema_version/run_id/capabilities/views`。Catalog按manifest顺序、最多64项且允许为空；compatible项
携带完整C05 `ViewRecord`，archive incompatible项只携带`status="incompatible"`、manifest的
`view_id/renderer`和C05 `IncompatibleView`，不得伪造title、descriptor或query。带`view_id`时保持既有query
response语义；其他无`view_id`参数fail closed。Catalog没有watermark、cursor或pagination，browser每个Run的
Views surface只原子取得一次；malformed/duplicate/run-mismatch使整个surface local-error，不部分接受。
Compatible项才发送query，incompatible项直接进入panel-local compatibility surface。

Active Production 中任何 invalid field/type、duplicate ID、unsupported descriptor/version 或超限 ViewSpec
都阻止 Production constructor/`start()` 并使进程非零，不能跳过一个声明视图后继续。读取 archive 时，
server/UI 不支持某条 record 的更高 `view_schema_version`，或某条 current-version record 自身损坏，只使
该 custom view 显示 normalized incompatible/corrupt reason；canonical event、内置 timeline、CLI query 和
其他独立兼容 view 必须继续可用。Archive loader必须先检查manifest声明的`view_schema_version`；高于当前
版本的record按opaque newer-schema data分类且不按current schema解析，只有current-version record的decode、
identity或canonical encoding失败才是`corrupt_record`。Manifest/store identity 或 canonical event storage 损坏仍是 archive
operation failure。任何 archive recovery 都不执行 Production Python。单次 invalid client query、断开或
renderer exception 只影响该 request/panel；query/server/store execution context 的系统性失效仍是 D16
的 Production-fatal core failure。

## 9. 存储、背压与敏感数据

### 9.1 持久化、背压、retention 与 archive

#### Per-Run SQLite store

V1 对每个 Run 创建一个独立 database：

```text
.troupe/diagnostics/runs/<run-id>/
└── diagnostics.sqlite3
```

Active store 使用 SQLite WAL、`synchronous=FULL` 和单一有序 writer。不同 Run 不共享 database、WAL、
writer queue 或 transaction；HTTP reader 使用独立 read connection/transaction，不直接参与 writer
serialization。Active 期间 SQLite 可以创建 `diagnostics.sqlite3-wal` 和 `diagnostics.sqlite3-shm`，所以
任何备份、cleanup 或移动都必须把整个 Run directory 当成单位，不能只复制主 database file。

Store schema 至少分成三类：

- `run_metadata`：`store_schema_version`、`run_id`、开始/结束时间、Production outcome、committed
  watermark、read-model watermark、配置 identity 和 `clean_shutdown`。
- append-only `events`：lossless sequence key、便于 kind/scope 查询的 typed index columns，以及完整的
  canonical `DiagnosticEvent` JSON。已经 commit 的 event row 不允许 update/delete。
- versioned materialized read-model tables：open/completed span、assembled agent message、latest plan/counter、
  usage aggregate 和 snapshot 所需状态。它们是可从 `events` 重建的查询加速数据，不能成为第二事实源。

Canonical JSON 中的 `u64` 仍按第 6.1 节保存为 decimal string。SQLite `INTEGER` 不能表示完整 unsigned
64-bit domain，所以 sequence、watermark 及其他需要数值排序的 `u64` physical key 使用 fixed-width
8-byte unsigned big-endian BLOB（或经兼容测试证明等价的 lossless sortable encoding），不能把 V1
悄悄缩窄为 signed 63-bit。`events` 同时保存 canonical decimal sequence，读取时必须验证 index key、
JSON sequence 与 Run identity 一致。

初始 durable transaction 写入 `committed_watermark="0"`、同值 read-model watermark 和
`clean_shutdown=false`；它同时是 registry 发布前的真实 store write check。此后每个 writer transaction
必须按 sequence 顺序原子完成三件事：append 一段连续 event、更新这些 event 对应的 materialized
state、把两个 watermark 一起推进到 batch 末尾。任一部分失败都 rollback 整个 transaction。因此任意
成功打开的 store 只能公开 sequence `1..W` 的 dense committed prefix，snapshot state 也恰好对应 `W`，
不会出现 event 已可 replay、snapshot 却只更新一半的状态。

#### Commit 与 crash boundary

Instrumentation/hub 完成 normalization、sequence assignment 和有界 queue admission 后即可返回，不为
每个 event 同步等待 disk。Writer 按以下 trigger 中最先到达者组成一个 transaction：

- oldest queued event 等待 25 ms；
- 512 events；
- 1 MiB canonical encoded event bytes。

这些值是 V1 fixed operational values，不提供 CLI/environment/Python tuning；实际值仍必须通过
identity/status endpoint 暴露，便于解释延迟和资源行为，但不属于 HTTP/wire compatibility。未来版本
可以引入显式 tuning contract。25 ms 是健康 writer 开始 commit 的 batching trigger，不是 scheduler、
filesystem 或异常 stalled writer 下的 wall-clock loss guarantee。

`COMMIT` 在 `synchronous=FULL` 下成功返回，是唯一的 committed/durable boundary。只有此后 writer 才
原子推进进程内 committed watermark 并通知 snapshot/SSE/exporter reader；commit 前的 queue event 不能
先出现在 UI。这里的 durability 受 SQLite、OS、filesystem 和 storage hardware 实际兑现的 guarantee
约束，Troupe 不声称能克服会谎报 flush 完成的硬件。

在正常 writer 路径中，committed transaction 经进程 crash 后必须恢复；在底层兑现 flush guarantee 的
前提下，machine/power loss 后也必须恢复。异常终止可以丢失已经被 hub 接受但仍在 queue 或未成功
commit transaction 中的 tail；这个 tail 的 event/byte 上界由 mandatory queue 限额约束，但不能承诺
固定毫秒数。Run 不以同一个 `run_id` resume，所以 recovery 后仍只有 dense prefix，不在其后续写。
Store 创建时已经 durable 写入 `clean_shutdown=false`；只有正常 final transaction 才改为 `true`，因此
archive reader 能明确看出是否可能存在未知 crash tail，而不是把 EOF 当成完整结束。

`clean_shutdown` 只表示 diagnostics producer 已 seal、terminal facts/final metadata 已完整 commit，和
Production 业务 outcome 正交。Production 因用户 exception 失败但 diagnostics 正常收束时可以是
`outcome=failed, clean_shutdown=true`；diagnostic writer 自身失效时 archive 通常保持
`clean_shutdown=false`，进程 outcome 必须非零。Hard crash 不会产生虚构的 `ObservationGap`，因为已经
没有仍在运行的同一 Run 可以观察未知尾部；incomplete marker 承担这一语义。

#### Mandatory backpressure

Mandatory ingress 的 accepted-but-uncommitted budget 默认同时受两个 hard limit 约束：32,768 events 和
64 MiB canonical encoded bytes，任一个先达到即为满。Budget 包含仍在 queue 和已经被 writer 取出但
transaction 尚未成功 commit 的 event；成功 commit 才为继续 admission 释放相应 capacity。Rolled-back
batch 如果仍可安全 bounded retry/drain 就继续占用 capacity；如果 writer 已不可恢复而放弃该 tail，Run
同时被永久 seal，不再利用释放出的空间继续业务或拼接后续 event。因此 crash-tail 上界不会因
“dequeue 但仍 in flight”被低估。

Hub 在同一个 admission critical section 内计算 candidate event 的 encoded size、预留 capacity、分配
下一 sequence 并 enqueue；capacity 预留失败不能消耗 sequence。Admission 不阻塞 Production 等待腾位，
也不使用无界 overflow buffer；下一个 event 无法 admission 时立即 seal 普通 ingress、报告 core failure、
停止新 Production/Cue work，并启动有限的 settlement/drain。如果 budget 后续恢复且 store 仍健康，
Runtime 只有先按序 commit 已接受的 tail 后，才可以用下一个连续 sequence 尽最大努力记录
infrastructure failure/`ObservationGap`；不能跳过失败 batch 直接写 terminal marker。无论记录是否成功都
不能恢复 Production。单 writer 必须按 sequence 消费；存在 uncommitted event 而 committed
watermark 在 configured writer-progress deadline 内不推进，同样视为 stalled writer failure。Progress
deadline 和 shutdown drain deadline 必须有限、可配置并通过 status 暴露；它们是部署/benchmark tuning，
不是 wire contract。

Queue exhaustion、writer task exit、transaction/flush error、store 不可访问、disk full、quota crossing
或 drain deadline 超时都使 Production 非零结束。Runtime 不能丢弃 core event 后继续，也不能把自身
writer loss 降级为普通 `ObservationGap`。如果 store 尚可用，可以尽最大努力提交 normalized fatal/gap
事实；不能再写时 stderr 与 non-zero process outcome 是最后保障，archive 保持 incomplete。

Web/SSE subscriber、per-Act Python sink 与按需 exporter 在 mandatory queue 之外使用各自的 bounded
delivery。它们的 overflow、callback/request/export failure 只使该消费者 incomplete/失败，不占用或
反压 mandatory writer，也不终止 Production。

#### Retention 与 shutdown lifecycle

V1 不在 active Run 内按时间、byte、Scene 或 event kind 裁剪历史；只要 Run directory 存在，它就保留从
sequence 1 开始的完整 committed prefix。可选 `max_run_bytes` 默认 unset；配置后，它作为 Run 的
diagnostic data budget，不能删除早期 event 来换取继续运行。`--diagnostic-max-run-bytes` 的 accounting
是 Run directory 内 validated regular files（包含 database、WAL/SHM 与 Troupe-owned metadata）的
apparent file length 总和，不跟随 symlink；Runtime 在 admission/batch 前做 conservative precheck，并在
每次 commit/checkpoint/file growth 后重新测量。Precheck 判断会越界或 post-write measurement 已达到/
越过 limit 都触发 core failure。它是 fail-closed operational budget，不保证 database overhead 永远不会
在最后一次 commit 中短暂越界；effective limit、current measured bytes 和 last measurement time 必须在
status 中显示。Filesystem capacity/I/O error 始终可能更早触发同样的 fatal contract。

正常 Production 结束按下面顺序收束：

1. 停止新的 Production/Cue admission，并在 diagnostics 仍健康时 settlement/cancel 已有工作。
2. 写入所有 Runtime-owned terminal lifecycle facts 和 Production outcome，然后 seal canonical ingress。
3. 在 bounded shutdown deadline 内 drain writer；final transaction 同时写 `ended_at`、最终 watermarks 和
   `clean_shutdown=true`。
4. 向当前 live subscriber 尽力发送带 final committed watermark 的 `stream_closed`，并结束其 reader。
5. unlink `instances/<run-id>.json` 并 durably sync `instances/` directory。
6. 关闭 listener/readers/writer/SQLite；不 daemonize，也不让 Runtime 为诊断页面无限存活。

步骤 2-6 的 core persistence、registry 或 server shutdown failure 都必须使 process 非零；若 final
transaction 未成功，`clean_shutdown` 必须保持 false。`stream_closed` 是 best-effort transport control，
peer 已断开本身不把一次已经 durable 完成的 archive 降级为 incomplete。

Completed 或 crash-incomplete Run archive 默认无限期保留，不自动按年龄、数量或总 byte 删除。只有
第 7.1 节的显式 CLI cleanup 可以应用 exact/age/run-count/total-byte policy；它只能在确认没有 active
instance 且取得该 Run exclusive archive lease 后，将整个 Run directory 从可发现 namespace 中移除。
Active Run、正在被 archive server/query/export 使用的 Run 和无法取得 lease 的 Run 必须跳过。不能只
删除早期 event、WAL 或某张表。

每个 Run directory 包含一个不承载业务数据的 lease anchor。Runtime 从创建 store 到关闭 store 持有
active exclusive lease；Runtime 内部 active HTTP/query handler 复用该 guard 且不重新加锁。CLI 对 active
target 必须走该 server，不能直接读 active store；只有 inactive local/archive
`status/snapshot/events/dump/serve` reader 持有 shared archive lease；
`cleanup --apply` 必须取得 exclusive cleanup lease。Active exclusive lease 阻止任何 CLI 绕过 live
server 直接读 store，shared lease 阻止 cleanup，cleanup lease 阻止新 reader。进程 crash 时 OS 必须释放
lease；lock acquisition/error 不能被解释为“无人使用”，必须保守失败。Exact cross-platform locking
primitive 与 anchor filename 是 implementation contract，但不能用只检查 lock file 是否存在来替代
process-owned lock。复制完整、静止 archive 后，copy 上没有进程持锁，可以独立读取。

Runtime server 随 Production 退出。之后本地 CLI archive 命令通过同一 read-only server/query
implementation 打开 Run store；需要 Web UI 时，由用户显式启动 foreground、loopback-only temporary
archive server，退出命令即关闭。Archive mode 不发布 Production `instances/` entry、不允许写 event，
也不能把 `clean_shutdown=false` 隐藏成正常 completed Run。Exact commands、selectors 与 cleanup policy
由 D29-D33 和第 7.1 节定义。

### 9.2 已确认的 V1 内容级别

默认记录 identity、结构、phase/outcome、稳定错误码、面向用户的 agent message、plan、tool
名称/状态、provider-neutral activity、usage 和有界业务 annotations。Agent message 进入 Run store
与 trusted-LAN 实时 UI，但 exporter 可以采用更严格的内容策略。下列内容默认排除：

- `Actor.act()` script、agent thought/reasoning 和 chain-of-thought。
- tool arguments、tool output、文件内容与环境变量。
- validated result 的字段值。
- provider raw protocol payload、credential 和认证材料。

面向用户的 agent message 仍可能包含 repository 或业务敏感信息，因此必须受 retention、已冻结的网络
部署边界和大小上限约束。Troupe diagnostics 不负责检查、识别、按 key 脱敏或改写 payload 的内容；
tool、agent 和启用敏感 capture 的调用方对其内容负责。Perfetto 默认只导出 message 时间点、message ID、
长度和 truncation/gap 状态，不导出正文。是否提供显式 sensitive-content opt-in、tool/result content
capture，以及 dump 时是否允许进一步剥离字段，需要单独设计。即使启用敏感内容，也必须有大小上限，
不能无限内嵌 blob。

## 10. 兼容性与故障边界

- Event schema、registry、HTTP API、live stream 和 `ViewSpec` 分别版本化，不能假设它们永远同步
  升级。
- UI 静态资源与 server 来自同一 Troupe build，必须拒绝无法解释的重大协议版本。
- Completed archive 只保存 canonical diagnostics，不保存创建它的旧 UI bundle；`diagnostic serve` 使用
  当前 Troupe build 的 embedded UI，在 bootstrap 时逐项验证 archive event/ViewSpec schema compatibility。
- Supported browser 在 bootstrap 后才打开 live transport；低于第 6.3 节 capability/browser floor 的客户端
  只得到静态 compatibility state。Asset encoding、cache validator 或 browser state 都不能改变 canonical
  query/event 解释。
- CLI 连接 registry 后校验 server identity、run identity 和 protocol compatibility。
- Active Production 的 Python view 编译失败属于 startup failure，在 `start()` 前形成 normalized error
  和 failed archive。只要 diagnostics writer/finalizer 健康，该 user/ViewSpec failure 必须完成
  `outcome=failed, clean_shutdown=true` 的 terminal transaction，constructor/start 均未执行，并 durable
  unpublish registry、关闭 listener/readers/store、释放 active lease；只有 diagnostics finalization 自身失败
  才保持 `clean_shutdown=false`。Archive-only unsupported view 只局部标记 incompatible，不能遮蔽 canonical data。
  单个 subscriber、client query/renderer 和 exporter failure 使用各自的局部 normalized surface，不冒充
  Production lifecycle failure；server、canonical pipeline、query execution context 或 persistent writer
  的系统性 failure 则是 Runtime infrastructure failure，不得伪装成某个 Actor turn 或用户 Production
  exception。
- 现有用户 Production exception 与 stderr formatting contract 保持有效。新增 diagnostic core failure
  使用独立、稳定的 infrastructure error surface，并使进程非零结束；exact exception/error code 随
  Runtime 实现 contract 冻结。
- Active Q00 reader 的 SQLite corruption、identity/dense-prefix invariant failure，以及 Q01/H03 query
  execution context/worker 的系统性退出都属于 core fatal signal，必须由 supervisor 取消 Production 并非零
  收束；同一 corrupt/incompatible archive 的 open/query 是对应 archive request/command error，不影响其他
  active Run。Client descriptor/tamper/timeout/cancel 和单次 renderer failure 仍只属于 request/panel-local error。

## 11. 验收场景

正式实现至少要覆盖：

- `.troupe/` 缺失时可创建；只读、被普通文件占用或实际写入 probe 失败时，在 Production import 前
  启动失败，且不回退到其他 state root。
- store write check、server listener 和 registry 在 Production 构造前全部 ready，
  能够观察构造期 Actor cast 和 session opening；任一步失败都清理部分资源并阻止用户代码运行。
- 现有 `troupe --production ... -- <args>` 保持兼容；六个 diagnostic Runtime flags 只在 separator 前解析，
  malformed size/duration/URL 或 bind failure 在 import 前失败。Registry ready 后 stderr locator 是单行
  versioned JSON、包含完整 identity/path/security scope，且 Production stdout 不受影响。
- 每个 Run 使用独立 WAL/database/writer；event、materialized state 和两个 watermark transactionally
  对齐。模拟任一中间 statement/commit failure 后只能恢复原 `1..W` dense prefix，不能暴露 partial batch。
- sequence/watermark 接近和越过 signed 64-bit 上限时，big-endian key、canonical decimal JSON、排序、
  cursor 和 snapshot 仍保持完整 `u64` 语义。
- 本机 registry discovery 覆盖零/单/多个并发实例；多实例时不使用 implicit latest，并要求显式选择。
- `diagnostic runs` 在零/active/definite-stale/unhealthy/identity-mismatch/invalid/incompatible/completed/
  incomplete 混合状态下完整列出 candidate；`--production [--run]` resolver 覆盖唯一 active 优先、多 active
  歧义、唯一 archive、多 archive、definite-stale 回退，以及其他 potentially-live 状态禁止 SQLite bypass。
- `--production`、`--url`、`--archive` selector 的互斥/required 规则、canonical UUID/URL validation 和
  archive-directory identity check 都不 import/构造 Production；故意放入会抛错/产生 side effect 的
  `__init__.py` 仍可执行 runs/status/snapshot/events/dump/serve/cleanup。
- normal shutdown 在 listener close 前撤销 entry，且不删除 Run store；crash 后分别覆盖 active、
  definite-stale 自动清理、live-owner/unreachable、run identity mismatch、invalid 和 newer-schema entry。
- trusted-LAN peer 无认证访问、plain HTTP、同源 UI/API/live stream、无 CORS opt-in，以及未配置/显式
  `advertise_url` 的本机和远端访问路径；页面与 identity 明确显示 trusted-network security scope。
- committed snapshot `W` 后从 `(W, H]` replay 并无缝进入 live tail；snapshot 与 stream 之间的新 commit
  不丢失，单连接严格递增，跨重连 duplicate 可按 `(run_id, sequence)` 去重。
- Event admission 但尚未 commit 时 UI/SSE/exporter 不可见；`FULL` commit 后才推进 watermark/notify。
  在 batch 的 queue、transaction 前/中/后注入 process crash，恢复结果分别验证 bounded unknown tail、
  dense committed prefix 和 `clean_shutdown=false`；正常 final transaction 则为 `true`。
- 初次 `after`、自动重连 `Last-Event-ID` 优先级、空 Run cursor `"0"`、非法/超前/不可恢复 cursor、
  `resync_required`、`stream_closed` 后停止自动重连，以及各 control frame 不推进 cursor。
- 慢 SSE client 的有界 buffer overflow 不静默跳 event：尽力发 `delivery_gap` 后断开，并从 store replay；
  response header、逐帧 flush 与 reverse-proxy no-buffering 保持流式交付。
- `diagnostic events` 覆盖默认 tail 100、tail 0、after 0、tail/after conflict、finite captured head、
  replay-follow handoff、archive follow rejection、断线 dedupe 和 identity change failure；JSONL stdout 只有
  严格递增且不重复的 canonical event，所有 control/warning/error 留在 stderr。
- runs/status/snapshot 的 human/JSON 与 events 的 human/JSONL schema、newline、完整 UUID、decimal-string
  `u64` 和 explicit null 验证；failed/incomplete Run 读取成功 exit 0，operation failure 1，usage 2，SIGINT
  130，并保证 machine stdout 不被 warning/progress 污染。
- 所有 event/control/snapshot 中 schema-declared `u64` 以 canonical decimal JSON string 往返，UUID、
  optional `null`、单 event 单 frame 和 SSE `id=sequence` 能跨 JavaScript/CLI 无损 round-trip。
- 一个 Scene 中同一 Actor 多个 completed/running/queued Cue 的独立分组，以及跨 Actor 并发。
- `Actor.act()` caller cancellation 后 remote turn 继续 settlement 的双 span 与 handoff flow。
- view pause 时继续 cursor/watermark ingestion，并准确显示 unseen sequence 数量；resume 从有界 hot window
  或 captured-range query 补齐呈现，UI 选择与展开状态不重置，且内存不随 pause 时长无限增长。
- Strict TypeScript protocol reducer 与 Rust 使用 shared canonical fixture 验证 decimal-string/`bigint`、
  optional field、unknown/incompatible schema 和 query invalidation；Preact/Canvas/uPlot 不产生另一套事实语义。
- Timeline 在 device-pixel Canvas 上只画 visible row/time range，DOM tree 与 track 同步 virtualize；10,000
  visible primitives 和 sustained live update 下每 animation frame 最多一次 draw，LRU/heap 保持有界且
  selection、span pairing、usage coverage 和 gap state 不丢失。
- Chromium/Firefox/WebKit 覆盖 desktop 与 mobile interaction、pan/zoom/follow、multi-Cue selection、
  screenshot 和 Canvas nonblank/pixel check；ARIA treegrid、keyboard-only selection、inspector text 与 axe
  检查证明 Canvas 不是唯一 semantic surface。
- Malformed、HTML/script-like 与超长 user content 只能成为 text；CSP、`nosniff`、no-referrer、same-origin
  resource policy、无 CORS、无 external request，以及不使用 inline script/`dangerouslySetInnerHTML` 均有
  browser/HTTP test。
- HTML/hashed asset/API/SSE 的 cache policy、Brotli/gzip negotiation、`Vary`、representation-specific ETag、
  `HEAD`/conditional request、exact MIME 和 reverse-proxy relative path 全部从 embedded bundle 验证。
- Clean frontend rebuild 与 checked-in generated assets byte/hash 相同；ordinary sdist/maturin/wheel build 在
  Node/npm 不可用且 network disabled 时成功。最终 wheel 没有 source map/loose UI asset，能启动 active 与
  archive UI，且 512-KiB/160-KiB/768-KiB 三个 release budget 都通过。
- `event()`/`counter()`/`span()` 的 name、severity、flat value、finite number、entry/list/canonical-byte
  上下限；eager copy 后修改原容器不改变 event，且任一 invalid call 在 sequence 分配前原子失败。
- Instrumentation 在 `start()`/Scene/Cue/Act/`stop()` 和 registered child task 中继承准确 domain scope；
  import/constructor、plain thread、unregistered task、expired scope 与 Run 结束后抛 `DiagnosticContextError`。
  Caller 不能覆盖 identity/time/parent/causality，也不能从返回值取得 canonical ID。
- `span()` 未 enter 不产生 event；normal/cancelled/failed exit 形成唯一 matching finish 且不吞 body
  exception。Custom span parent 只在同一 task 传播，新 child task 继承 domain scope 但不继承 custom
  temporal parent。Gauge counter 的相同 series、latest selection 和 event-count 替代 delta 语义一致。
- Custom event admission 成功后与 built-in event 遵守同一 mandatory persistence/fatal backpressure
  contract；进入 Act scope 的 custom event 在 sink capture on/off 时分别投影/排除，但 store、Web 与
  Perfetto canonical 事实不变。
- 四种 ViewSpec 的 frozen/final construction、exact class tuple、unique ID、static lookup、closed query/
  reducer compatibility 和 viewport/run、selection/run binding。SQL/regex/join/callable/custom renderer/
  executable markup 被拒绝，所有业务字符串按 text 渲染。
- ViewSpec 在 Production class resolution 后、constructor 前编译并持久化；duplicate/invalid/current-version
  incompatible spec 阻止 active Production 构造与启动。Archive 遇到 newer view schema 时 custom panel 局部 unavailable，
  canonical diagnostics、内置 UI 和兼容 view 仍可读且不 import Production。
- Query captured watermark、cursor pagination、latest gauge、completed-span duration、token coverage、
  missing/non-numeric/open/gap exclusion 和 result completeness；TimeSeries 的 watermark、viewport/range 或
  derived width 任一改变都触发整份 refetch，旧 inflight response 不能覆盖新 binding，browser 不 merge 或
  rebucket 不同 width 的结果；single request/renderer failure 保持局部，query server 的系统性 failure终止
  active Production。
- `DiagnosticCapture` 默认值、strict `bool` validation、tool content dependency 和不可关闭的 lifecycle/gap；
  sink base initialization、成功 admission 后的一次性 bind、三个稳定 state-error code，以及 sync preflight
  failure 不消耗 sink。
- Per-Act sink 的 message/tool/plan/context usage 顺序；`wait_closed()` 可重复返回同一 immutable summary，
  waiter cancellation/外层 timeout 不取消 sink 或 Act；callback 抛错、自发 `CancelledError`、invalid return、
  无限慢 sync callback 和 Runtime shutdown 分别形成准确的 failure/abandoned/complete 状态。
- 每 Runtime 一个 dedicated diagnostic thread/loop、每 sink 串行且跨 sink 可交错、空 `contextvars.Context`
  和无 Cue/Actor authority；验证 callback latency/failure 不阻塞 Production loop 或改变 Act settlement。
- sink 的 1,024-event/8-MiB budget、其中 32-event/256-KiB structural reserve，以及 Runtime 总计
  16,384-event/64-MiB budget 的每个边界；低优先级 drop、reserve exhaustion、按 kind/byte 计数和
  capture-relative `complete` 必须 deterministic。
- 系统级 sink delivery-fact 验收必须证明 callback/unexpected-enqueue 首次故障在 store、HTTP/Web query 与
  CLI 中各由同一个 sequence 恰好可见一次，且不回投任何 per-Act sink；普通 drop 只产生累计
  `diagnostic.dropped_events` 和 summary，不产生 component failure，counter delivery 失败也不递归。
- 同 message adjacent delta 在 16 KiB、20 ms、同 Act 下一 canonical event 和 turn terminal 处 flush；
  sequence 分配后 store、Web/SSE 与 sink 观察到完全相同的 event，任何 subscriber 不再 merge/rewrite。
- missing-ID chunk 跨 tool/plan/usage/reasoning interleave 仍属于同一 anonymous message；anonymous 与 explicit
  message 可并存，terminal 按首次出现顺序关闭；provider ID 完成后复用会分配新 Run-local ID 并产生 gap。
- tool input/output opt-in 保持 opaque 且不扫描、脱敏或改写；depth/node、单 snapshot、per-Act tool、
  per-message/per-Act agent text 和 plan snapshot 的精确上下限均验证 atomic omission、显式 truncation 与
  summary incomplete，且不能产生 partial invalid JSON。
- Context occupancy 跨 Act 累积和 compaction 下降时不被误报为 per-Act token usage；provider usage
  缺失时字段保持 absent。
- 每个 `SpanStarted(span_kind="act.lifecycle")` 都有且只有一个 `ActTokenUsageFinalized`，并且位于对应
  `SpanFinished` 之前；分别在线性化的 pre-submission Act terminal、submitted-without-settlement session
  terminal、authoritative settlement 三条路径覆盖 prompt 未提交、正常完成、result repair、max tokens、
  caller cancellation 后 supervisor settlement、session terminal 和 provider 不支持。unavailable 与
  provider 直接报告的零保持不同，racing terminal/settlement 只能消费一次 finalization slot。
- 固定 Codex 与 Claude adapter 分别验证单 model request 和带 tool loop/多 model request 的 Act，证明
  `PromptResponse.usage` 是整个当前 turn 的 final accounting 后才能标记 available。固定 Kimi adapter
  在未通过等价验收前标记 unavailable；不能读取 agent 私有日志或内部 session 文件补数。
- available/partial/unavailable 与四个 unavailable reason 的全部合法组合和非法组合；token 字段接受
  非负 Python `int`、拒绝 `bool`/负数，且不宣称或测试 Troupe `u64` product maximum。Provider total 不由
  breakdown 合成，可选分类缺失不降级 available。
- 各 token 字段独立的 known sum、reported/finalized coverage 和 availability counts；wire decimal string
  跨 JavaScript 无损 round-trip，Run/Scene/Actor known totals 不冒充完整 totals。
- Caller cancellation 后 sink 继续观察 supervisor-owned remote turn，直到 settlement 或 session
  terminal；caller 与 turn completion 不被合并。
- 运行中 watermark dump 与运行结束后的 dump 都经过三层 blocking compatibility gate：独立 protobuf
  implementation decode byte-exact golden；按 release SHA-256 固定的 official v57.2
  `trace_processor_shell` 对 track/slice/counter/flow/args/metadata/stats 执行 SQL assertions；以及 pinned
  official Perfetto UI browser screenshot/pixel smoke。CI 工具只在 dedicated job 下载，不能进入 wheel。
- Perfetto fixture 覆盖 empty/open/nested/multi-Cue span、需要 deterministic sibling lane 的 non-nested
  overlap、equal timestamp、Unicode、annotation、`ObservationGap`、flow、exact/non-exact counter、numeric/
  ID-space boundary、malformed reference、active/archive watermark 和 repeated byte-identical dump。
- Scheduled CI 可以用 current public `ui.perfetto.dev` 做 non-blocking canary；网络失败或独立演进的 public
  UI change 不能使同一 release candidate 非确定地失败。Pinned offline tests 才定义 release correctness。
- `diagnostic dump` 覆盖默认 captured head、explicit through/0/future rejection、active/archive target、
  output exists/force、directory/symlink refusal，以及 encode/fsync/rename/backup/rollback/cleanup failure
  injection；成功不留下 temp/backup，失败必须区分 durably restored `not_published` 与
  `publication_indeterminate`，不能无证据声称旧 output 未变，成功 trace metadata 与 requested watermark 一致。
- Schema audit 覆盖 pinned upstream proto/license/hash/closed used-definition manifest，逐项验证实际镜像
  message/field/enum及其 selected type dependency，并明确不要求未选 import target；ordinary build 在 network、Node、
  `protoc` 与 Perfetto binary 均不可用时仍成功。Wheel 不新增 Python dependency、loose schema/asset、external
  executable/shared library 或 ELF `DT_NEEDED`；CI 记录加入 exporter 前后的 wheel/native-module bytes 作为
  informational artifact，不设 fixed size acceptance threshold。
- server execution context 或 persistent writer 在 Run 中退出、磁盘满/权限撤销/commit error，以及
  core queue 持续过载，都会停止 Production 并产生非零 outcome；尽可能保留 fatal/gap 证据。
- Active reader corruption/dense-prefix violation 与 query worker/execution-context death 同样停止 Production
  并非零；archive corruption、invalid client query、timeout/cancel 保持 request/command-local，failure matrix
  断言两类不混。
- 32,768-event/64-MiB queue 的任一边界、25-ms/512-event/1-MiB batch trigger、writer progress timeout、
  shutdown drain timeout 和 configured `max_run_bytes` 都有 deterministic overload/failure tests；没有路径
  能 drop core event 后继续 Production 或退化为 unbounded memory。
- Active Run 从 sequence 1 保留完整历史；normal shutdown 完成 terminal commit、`stream_closed`、durable
  registry unpublish 和 resource close，且 server 不留驻。User Production failure 可形成
  `outcome=failed, clean_shutdown=true`，diagnostic finalization failure 保持 false 并非零退出。
- Completed/incomplete archive 默认保留并能被同一 query semantics 读取；cleanup 只删除整个 inactive、
  unleased Run，跳过 active/leased archive，删除一个 Run 不影响其他并发 Run。
- Runtime active exclusive guard 复用于 active query/dump 而不二次加锁；archive query/serve/dump shared 和
  cleanup exclusive lease 覆盖 crash release、reader/
  cleanup race 与 copied archive；`serve` 只绑定 loopback、默认 port 0、前台退出释放 lease、不发布 registry，
  `--open` 是唯一 browser side effect。
- `cleanup` 的 exact/age/keep-count/total-byte preview 与 apply 使用 deterministic ordering/size accounting；
  batch policy 保护 incomplete archive，apply 前重新检查 identity/lease，atomic undiscovery 后 whole-directory
  deletion 不跟随 symlink，protected data 超 budget 时报告无法满足而不误删。
- 浏览器断开、单个 HTTP request、慢客户端、Python sink overflow 或按需 exporter failure 不会停止
  Production，并且对应的局部 delivery gap/incomplete/error 可见。
- 默认导出不包含排除的敏感 payload。

## 12. 待决问题

原 Item 1（`DiagnosticEvent` taxonomy、字段、span 与默认内容）已由 D13-D15 和第 4 节解决；
原 Item 2（server/registry 启动与运行故障、state root 可写性、启用策略和 bind/port）已由 D16-D18
及第 3、5、9、10 节解决；原 Item 3（per-Run registry、并发发现、发布/撤销和 stale cleanup）已由
D19-D20 和第 5.2 节解决；原 Item 4（`advertise_url`、no-auth trusted-LAN、同源/CORS 和 TLS/proxy
边界）已由 D21-D22 和第 5.3 节解决；原 Item 5（SSE、snapshot/replay/live cursor、control frame 和
JSON wire encoding）已由 D23-D24 和第 6.1 节解决；原 Item 6（per-Run SQLite schema、durability、
backpressure、retention 和 archive lifecycle）已由 D25-D28 和第 9.1 节解决；原 Item 7（CLI command、
target resolution、output/exit contract、archive serve/cleanup 和 Runtime flags）已由 D29-D33 及第 5、7、
9 节解决；原 Item 8（`DiagnosticSink` public API、delivery isolation、message normalization、queue 与
payload resource limits）已由 D34-D38 和第 4.5 节解决；原 Item 9（`ActTokenUsageFinalized` public
contract、availability/source/reason、aggregation coverage 和 sink summary 边界）已由 D39-D40 和
第 4.5、6.1 节解决；原 Item 10（Python instrumentation API、custom namespace/value/resource rules、
首批 `ViewSpec` renderer、closed declarative query 和 extension failure isolation）已由 D41-D44 和第 8、
10 节解决；原 Item 11（Web UI stack、Canvas/DOM rendering boundary、有界 browser state、deterministic
build、wheel embedding、HTTP/security/browser contract 和 release verification）已由 D45-D49 和第 6.2、
6.3、10、11 节解决；原 Item 12（Perfetto 最小 protobuf/TrackEvent encoder、确定性映射、numeric
projection、打包和 compatibility strategy）已由 D50-D54 及第 7.2、10、11 节解决。

当前没有未解决的编号设计项。现有 UI demo 仍只作为信息架构和交互 baseline，不能直接提升为 protocol
或 production implementation；后续进入 implementation plan 时必须以本文 accepted contract 为准。
