# Actor.act() ACP Agent Session 设计

- 状态：Accepted
- 设计版本：V1
- 实现状态：实现完成
- 协议基线：ACP stable v1；adapter-pinned stateful MCP revision + `McpTransportProfileV1`
- 最后更新：2026-08-11

## 0. 目标与设计摘要

### 0.1 目标

本文定义 `Actor.act()` 的 public API、agent session 生命周期、ACP backend、结构化结果
contract、取消与故障语义、provider 支持边界和验证方式。V1 必须满足以下目标：

- 每个 Actor 拥有一个与自身 capability 同寿命、跨 Cue 和 Scene 保留上下文的真实 agent
  session。
- `cast_actor()` 保持同步，agent session 在 Runtime 后台打开；首次 `act()` 必要时等待同一
  readiness。
- agent 通过 Troupe 注入的 HTTP MCP tool 返回结构化值，Runtime 使用同一份 typed contract
  生成 prompt、校验结果并物化 Python `dict`；常用约束走 native fast path，用户可以通过
  public `SchemaValue` 编写长尾 prompt/validation 规则。
- 所有 session 都按无人值守、可超长期运行的 flow 执行，不依赖人工审批，也不通过隐藏
  timeout 推断失败。
- Codex、Claude 和 Kimi 通过各自的 ACP adapter 保留 provider harness。本文假设未来
  release 流程会保证发布包中的三个 adapter 可用；该流程留待后续单独设计。

目标 public API 示例：

```python
from troupe import act_schema

result = await self.act(
    script="do something",
    output_schema={
        "decision": act_schema.StrValue(
            description="对当前工作的最终结论",
            choices=("approve", "request_changes"),
        ),
        "score": act_schema.Int64Value(
            description="结果质量评分",
            min=0,
            max=100,
        ),
    },
)
```

每个 Actor 拥有一个有上下文的真实 agent session。`act()` 向该 session 提交一轮工作；
agent 通过 Troupe 注入的内部 HTTP MCP tool 提交结构化结果，Runtime 按本轮编译契约校验
后返回已验证的 Python `dict`。assistant 最终文本在完成协议分类后直接丢弃，不在 Runtime
中保存，也不参与返回值提取。

ACP stable v1 是 V1 唯一的 agent backend 协议。`agent_instance`、PTY parser 和自动
fallback 不属于本设计。

### 0.2 核心决策

1. ACP stable v1 是第一版 `Actor.act()` 的正式 agent backend 协议；每个内置 adapter 在 private
   registry 中固定一个 stateful MCP wire revision 与本文定义的 `McpTransportProfileV1`。Codex
   `codex-acp@1.1.9` 固定 `2025-06-18`；当前 Claude/Kimi spec 固定 `2025-11-25`。每条 route
   只接受其 adapter pin，不在运行时协商或 fallback 到另一 revision，也不接受 `2026-07-28`
   stateless MCP lifecycle。
2. 一个 Actor 对应一个独立的 ACP agent process、connection 和 session。
3. session 的上下文与 Actor capability 同寿命，跨 Cue、跨 Scene，并跨同一进程内
   的 `Runtime.run()` rebind 保留。
4. `cast_actor()` 保持同步，只提交后台 session 创建请求；`act()`
   在 session 尚未 ready 时等待同一个 readiness。
5. `cast_actor()` 必须通过 keyword-only `agent_profile` 直接接收一个
   `AgentProfile` 对象；`agent`、`workspace`、`model` 和 `effort`
   四个参数都必须显式提供，其中 `effort` 可以为 `None`。Production
   没有对应 default。
6. 同一 Actor 同时只允许一个 in-flight `act()`。并发调用 fail-fast，
   不进入 Actor FIFO。
7. `act()` 只允许在当前 Actor 的 active cued authority 中调用。
8. `output_schema` 是 Troupe 自有的 typed schema system。内置 `SchemaValue` 集合是 closed、
   native-compiled contract；public `SchemaValue` 同时提供显式的 Python programmable
   extension。Runtime 在同步 preflight 冻结 description、choices、rendered prompt 与 native
   contract；result 先经过 Rust decode/resource/JSON-kind/内置约束，再按需进入 caller-owned
   Python validation bridge。同一 graph 决定 prompt、result acceptance 和最终 PyO3
   conversion，但 custom `render_prompt()` 与 `validate()` 的语义一致性由扩展作者负责。
   agent 必须调用 Troupe 注入的 result MCP tool；Troupe 不解析 assistant 最终文本，也不依赖
   ACP/provider structured output 或 public JSON Schema。
9. runtime 不得为了恢复、负载均衡或重试，静默把 Actor 切换到新的空白 session。
10. Actor agent session 永远按无人值守方式运行。ACP permission、plan review 或
    provider question 都不能暂停等待人类；Troupe 自动给出协议响应。
11. 第一版采用 trusted execution model，不提供或承诺 Troupe sandbox。Agent 的实际
    能力来自 Troupe 进程的 OS 身份和外部部署边界；`workspace` 只是 cwd，不是 jail。
12. `Ready` 是 session 重建边界：Ready 之前，明确可重试的 Opening failure 可以在
    后台重建 process/connection/session；Ready 之后绝不以新 session 冒充恢复。
13. post-Ready session 是否可复用只取决于 authoritative turn settlement。若已经观察到
    terminal/protocol/transport/process event，却仍无法证明原 prompt 已唯一终止，则必须
    kill/reap 并进入 Actor-lifetime `Broken`；纯沉默不构成 failure event，supervisor-owned
    remote turn 可以无限等待。
    第一版不自动 reconnect、load 或 resume。
14. Production 只启动一个共享的 loopback HTTP MCP listener。端口通过绑定
    `127.0.0.1:0` 由操作系统原子分配，再从仍然存活的 listener 读取；不扫描端口范围，
    不创建 socket 文件。
15. 第一版使用固定、版本化的 `ResourceLimitsV1`，不进入 `AgentProfile` 或 per-act
    override。HTTP listener 只设置 Production-global 65,536 live connections 上限，不做
    per-Actor connection limit；第一版不实现 agent process 或 turn capacity scheduler。
16. Runtime 不保存 per-turn assistant/update history。correctness evidence 保存在独立的
    bounded state 中；普通 update 在完成 protocol classification 后直接丢弃。
17. prompt 提交后的 caller/Cue cancellation 不等待 provider settlement。Runtime 先把 exact
    turn 原子移交给 Actor session supervisor，再让 `_ActCall` 抛
    `asyncio.CancelledError` 并结束 Cue child；supervisor 保留 session-turn lease，直到
    authoritative settlement、明确 failure 或 Production shutdown。第一版不增加 public
    per-Actor `close()`、`retire()` 或 `destroy()`。
18. public agent exception 使用以 `AgentError(RuntimeError)` 为根的小型层级，并以 closed
    normalized `code` 区分具体原因。preflight 继续使用现有 `CueContextError`、`TypeError`、
    `ValueError`；custom schema callback 的本地执行错误使用
    `act_schema.SchemaCallbackError`；cancellation 原样使用 `asyncio.CancelledError`。
    usage/trace/provider raw error 和 cleanup failure 不进入 `act()` exception contract。
19. 第一版不支持跨 Troupe process 的 Actor/session persistence，也不恢复 post-Ready
    `Broken` session；没有 public `load()`、`resume()` 或 `recover()`。进程重启后创建的
    同名 Actor 是新的 identity 和全新 agent session。未来持久化必须与 Production/Actor
    checkpoint、durable turn journal、result commit 和副作用幂等一起系统设计。
20. V1 public agent 集合固定为 `codex`、`claude` 和 `kimi`，每个值都映射到当前 build
    内置的 adapter；没有 public `available`/`experimental` 等中间等级，也没有版本或启动器
    override。本文直接采用第 0.1 节的未来 release 保证前提。

### 0.3 术语

| 术语 | 本文含义 |
|---|---|
| Actor agent session | 一个 Actor 独占的 agent process、ACP connection 和 ACP session |
| Opening attempt | session 首次达到 `Ready` 前的一组 preparation、process、connection、session 和 result-route 资源 |
| readiness | Actor-owned、single-assignment 的 startup 结果；transient retry 不提交 terminal readiness |
| caller admission | 同一 Actor 最多一个 public `act()` caller 的 fail-fast admission ownership |
| session-turn lease | exact remote turn 的唯一所有权；caller 取消后可由 supervisor 继续持有 |
| authoritative settlement | pinned adapter 能证明原 `session/prompt` 已唯一结束、tail updates 已收齐且没有 unresolved reverse request 的证据 |
| `ResultSlot` | 当前 Actor route 上按 exact operation arm 的单轮结构化结果状态 |
| supervisor handoff | prompt 提交后，将 exact turn 从 caller 原子移交给 Actor supervisor 的本地 ownership transition |
| custom validation bridge | 当前 `_ActCall` 把 native MCP validation request 调度到 active Python loop 的 caller-owned bridge；handoff 后关闭 |
| adapter contract | provider、ACP executable、internal mode/application、ACP/MCP wire profile 与 settlement rule 的 build-internal 组合 |

## 1. 产品契约

### 1.1 最小 Python API

Public API 形状：

```python
class Actor:
    async def act(
        self,
        *,
        script: str,
        output_schema: dict[str, act_schema.FieldSpec],
    ) -> dict[str, JsonValue]:
        ...
```

`Production.cast_actor()` 增加一个必填参数：

```python
class Production:
    def cast_actor(
        self,
        actor_type: type[Actor],
        *,
        name: str,
        agent_profile: AgentProfile,
        actor_args: tuple,
        actor_kwargs: dict,
    ) -> ActorHandle:
        ...
```

`agent_profile` 没有默认值，也不接受 Production registry 中的字符串名称。
调用方直接传入 `AgentProfile` 对象。缺少参数或对象类型错误时，cast 在发布
Actor 之前同步失败。

同一个 immutable `AgentProfile` 对象可以传给多个 `cast_actor()`
调用，但它只是可复用配置。每次 cast 都形成自己的内部 snapshot，并启动独立 ACP
process、connection 和 session。

`JsonValue` 概念上是：

```python
JsonScalar = None | bool | int | float | str
JsonValue = JsonScalar | list["JsonValue"] | dict[str, "JsonValue"]
```

`act_schema.SchemaValue` 是 public、abstract、可继承的 value 基类。所有内置 Value 都派生自
它；`act_schema.FieldSpec` 是 type-stub 中的 `SchemaValue | Field` 概念类型，其中
`Field` 只用于 object field position 控制 presence。root 使用 exact `dict`；nested fixed
object 使用 `ObjectValue`，因此每个实际字段 value 都能携带 description。

第一版 public shape 例如：

```python
from troupe import act_schema

output_schema = {
    "decision": act_schema.StrValue(
        description="审查结论",
        choices=("approve", "request_changes"),
    ),
    "int_field": act_schema.Int64Value(
        description="0 到 100 的整数评分",
        min=0,
        max=100,
    ),
    "float_field": act_schema.Float64Value(
        description="归一化置信度",
        min=0.0,
        max=1.0,
    ),
    "enabled": act_schema.BoolValue(description="是否启用该结果"),
    "tags": act_schema.ListValue(
        act_schema.StrValue(description="单个分类标签", max_length=32),
        description="与结果相关的分类标签",
        min_items=1,
        max_items=10,
    ),
    "metadata": act_schema.ObjectValue(
        description="可选的结果补充信息",
        fields={
            "note": act_schema.Field(
                act_schema.NullableValue(
                    act_schema.StrValue(description="补充说明"),
                ),
                required=False,
            ),
        },
    ),
}
```

root `dict` 表示 fixed object。bare field 默认 required，extra field 默认且第一版始终
拒绝；optional presence 与 nullable value 是两件不同的事。成功时直接返回经过 contract
校验的 Python `dict`，其全部业务字段都来自 `output_schema`。第一版不返回 `ActResult`
wrapper、`(value, metadata)` tuple，也不向 dict 注入 reserved metadata key。usage、effective
model/effort、provider session ID、operation/trace ID 和 timing 等运行信息不属于 `act()`
返回契约；它们留给后续独立设计的 diagnostic/observability 体系。

### 1.2 连续上下文

下面两个调用必须落在同一个 provider session：

```python
layout = await self.act(
    script="Inspect the repository and remember its package layout.",
    output_schema=layout_schema,
)

target = await self.act(
    script="Using what you learned, choose the module to change.",
    output_schema=target_schema,
)
```

第二轮能够使用第一轮留下的 conversation、tool state、provider session identity 和
workspace observations。两个 Actor 即使使用同一个 provider、model、workspace 或
profile，也不能共享 session。

这里的“同寿命”指逻辑所有权与 `ActorCapability` 一致。Actor 销毁后，
异步进程清理可能短暂继续，但 session 已经不再可查询、不可恢复、不可接受新 turn。

### 1.3 authority 与并发

`act()` 必须复用现有 Actor authority：

1. Actor 仍连接到自己的 `ActorCapability`。
2. Production 当前有 active `RunBinding`。
3. current task lineage 属于 active `CuedScope`。
4. 该 scope 的 Actor identity 与 `self` 相同。

registered descendant task 也拥有同一个 cued authority，因此“只能在 cued 域调用”
并不自动等于串行。下面的两个调用都可能通过 authority 校验：

```python
await asyncio.gather(
    self.act(script="first", output_schema=schema),
    self.act(script="second", output_schema=schema),
)
```

并发规则：第一个完成 admission 的调用取得 Actor 唯一 caller-admission token；另一个
立即以 busy error 失败。Troupe 不根据 task scheduler 的偶然顺序建立隐式 FIFO。

取得 caller admission 的调用可以等待该 Actor 的 startup readiness；若上一轮已因取消移交
supervisor，还可以等待同一个 lossless session-availability barrier。这个 barrier 只允许当前
唯一 admitted caller 等待，不保存 call request，也不是 FIFO。上一轮 supervisor 释放
session-turn lease、session 回到 `Ready` 后，该调用才能提交 prompt。

第一版没有全局 agent process 或 turn capacity scheduler。部署方需要的总进程/CPU/内存
约束由外部运行环境负责。

## 2. ACP 在系统中的位置

### 2.1 总体结构

```text
Python Actor.act()
        |
        v
Troupe authority / lifecycle / result contract
        |
        v
Actor-owned AgentSessionSlot
        |
        v
ACP client (official Rust SDK, stable wire v1)
        |
        v
dedicated stdio ACP agent process
        |
        +-- codex-acp ---------> Codex App Server / Codex core
        +-- claude-agent-acp --> Claude Agent SDK / Claude Code core
        +-- kimi acp ----------> Kimi Code harness

provider MCP client
        |
        | HTTP + Actor session-attempt bearer capability
        v
127.0.0.1:<OS-assigned-port>/mcp
        |
        v
Production-owned ResultMcpService (adapter-pinned stateful MCP / McpTransportProfileV1)
        |
        v
Actor ResultSlot -> native typed validation -> optional Python custom validation bridge
```

ACP 是 client 与 agent 之间的 typed JSON-RPC 协议，不是模型服务，也不是 agent
harness。Troupe 是 ACP client；Codex、Claude、Kimi 的 ACP executable 是 agent。

### 2.2 各层职责

| 层 | 负责 | 不负责 |
|---|---|---|
| Troupe | Actor/Cue authority、session ownership、readiness、caller admission、session-turn lease、取消传播、非交互 ACP callback、内部 HTTP MCP result service、native schema 校验、custom validation 调度、公开错误 | provider 内部 agent loop 与用户 custom validator 的业务正确性 |
| ACP SDK/protocol | framing、request correlation、capability negotiation、session/prompt/update/cancel、stdio process 通信 | Actor 生命周期、输出 schema、最终文本选择策略 |
| MCP protocol/server | exact adapter-pinned stateful lifecycle、result tool discovery、Streamable HTTP request/response、tool execution result | Actor authority、当前 act schema、ACP turn settlement |
| provider ACP agent | provider session、model/tool loop、project instructions、MCP client、provider auth 与 native events | Troupe 的 Python 返回契约 |
| 部署环境（Troupe v1 契约之外） | 可选的 container、VM 或独立 OS account 隔离 | Actor/ACP 运行语义 |

### 2.3 Harness 保留的准确含义

ACP 替换的是交互前端，不是 provider 的核心 agent runtime：

- Codex ACP adapter 启动 Codex App Server，沿用 Codex core 的 thread、turn、tool 和
  project-instruction 路径。
- Claude ACP adapter 使用 Claude Agent SDK，并显式启用 Claude Code preset、setting
  sources、tools、hooks、MCP 和 subagent 能力。
- Kimi 的 `kimi acp` 是 CLI 内建入口，直接使用 Kimi Code harness。

因此预期保留的是核心 harness，而不是 TUI 的每个可见功能。slash command、登录 UI、
交互式 permission UX 或 adapter 尚未映射的新功能不保证自动出现。每个固定版本组合
都必须通过 harness conformance test，不能把“支持 ACP”直接等同于“行为完全一致”。

## 3. Runtime 拓扑与所有权

### 3.1 一个 Actor 一个 process

第一版固定以下一一对应关系：

```text
ActorCapability
  1 -> 1 AgentSessionSlot
  1 -> 1 ACP subprocess
  1 -> 1 ACP connection
  1 -> 1 ACP session_id
```

ACP 协议允许一个 connection 承载多个 session，但第一版不共享 process，原因是：

- Actor 与物理资源的销毁边界一致。
- 一个 provider crash 不会同时破坏多个 Actor。
- 无需先证明多 session 的权限、cwd、环境变量和 close 相互隔离。
- 当前 Kimi 没有 `session/close` 也不构成阻塞；关闭专属 connection 和
  process 就能释放该 Actor 的资源。

以后可以把多个逻辑 session 合并到共享 process，但那只是经过 conformance 验证后的
资源优化，不得改变一个 Actor 一个逻辑上下文的公共契约。

`ResultMcpService` 是例外：它不是 agent process 或 conversation session，而是
Production 内所有 Actor 共享的本地 control-plane listener。多个 Actor 可以通过同一个
端口并发提交结果，但 bearer capability、MCP session generation 和 `ResultSlot` 相互
独立；共享 listener 不改变上面的一一对应关系。

### 3.2 Troupe 对象归属

| 对象 | Owner | 原因 |
|---|---|---|
| `AgentSessionSlot` | `ActorCapability` | capability 是跨 Cue、Scene、RunBinding 的稳定 Actor 身份 |
| `AgentSupervisor` | `ProductionState` 或更长寿的 native service | cast 可发生在 active Runtime 之外，需要独立于 Python event loop |
| `ResultMcpService` 与 live TCP listener | `ProductionState` 或同寿命 native service | URL 必须跨 RunBinding 稳定；当前 build 只有一套 MCP server implementation/profile；端口从 bind 到 shutdown 始终被持有 |
| Actor result route/capability | `AgentSessionSlot` | 只允许当前 session generation 向当前 Actor 提交结果 |
| `AgentTurnOperation` 的 caller-owned phase | active `CuedScope` | prompt 提交前及正常执行时，turn 参与当前 Cue/Scene 的结构化取消和 drain |
| `PythonSchemaValidationBridge` | caller-owned `_ActCall` / active `RunBinding` | custom callback 只能在发起本轮 `act()` 的 Python loop 执行；不属于长期 Actor session，handoff 时关闭 |
| `AgentTurnOperation` 的 handed-off phase | Actor slot 的 `AgentSupervisor` | caller 取消后继续唯一拥有 exact remote turn，且不再阻塞已取消的 Cue |
| ACP process/connection | Actor slot 的 cleanup lease | Rust `Drop` 不能 await，物理清理需在逻辑销毁后收敛 |
| provider profile | Actor slot 内的 immutable snapshot | provider/model/workspace 定义 session identity，不能逐 turn 漂移 |

`RunBinding` 只负责当前 event loop 和 task lineage。把 ACP session 放进
`RuntimeCore`、Scene 或 Cue 会在 rebind 或 scope 结束时错误地清空 Actor
上下文。

slot 必须持有一个即使 `ProductionState` 已经不可升级也能执行 cleanup/kill 的 supervisor
sender 或 cleanup lease。不能等到 `ActorCapability::drop()` 时再通过它现有的 weak
production edge 临时寻找清理服务。

### 3.3 不采用 `agent_instance` 与 PTY fallback

`agent_instance` 不再属于第一版架构，也不是 ACP 失败时的自动 fallback。
Codex、Claude、Kimi 均走 ACP。

若未来要支持没有 ACP server 的 CLI，应重新设计一个明确的 compatibility backend，
单独确认 exact-turn、取消、进程安全和维护成本。不能在 production 中静默从 ACP
切到 PTY parser，因为两者对上下文、结果边界和失败恢复的保证不同。

## 4. Actor 创建与后台 readiness

### 4.1 同步 cast transaction

`cast_actor()` 不等待 provider ready。同步 transaction 顺序是：

1. 校验 Actor type、name、构造参数，以及必填的 `AgentProfile` 对象；验证
   显式提供的 `agent`、`workspace`、`model` 和 nullable `effort`，再将
   profile 解析、校验并冻结为本 Actor 的 internal snapshot。这里没有
   Production、process-cwd 或 provider-model default lookup。
2. reserve Actor name，构造 Python Actor、`ActorCapability` 和
   `AgentSessionSlot(Opening)`。
3. 向 native supervisor 提交一次 owned open request，并取得可取消的 startup lease。
4. 完成 capability node、handle 和 registry publication。
5. 同步返回 `ActorHandle`。

本地 request 无法提交时，cast 同步失败并回滚 name/capability。request 已提交但后续
本地 publication 失败时，startup lease 必须关闭该 request 和已启动的进程。

package preparation、child process spawn、ACP handshake 和 session config
validation、result MCP initialize/tool discovery 发生在后台。缺少本机 `npx` 等可同步
确认的 launcher 错误在 cast transaction 中失败；更深层的启动结果由 shared readiness
交给第一次或后续 `act()` 观察。

### 4.2 Session 状态机

```text
Opening -> Ready
Opening -> AuthRequired
Opening -> StartFailed
Opening -> BackingOff -> Opening

Ready -> Broken
Ready -> Active
Active -> Ready
Active -> Cancelling
Active -> Broken
Cancelling -> Ready
Cancelling -> Broken

Opening | BackingOff | Ready | Active | Cancelling | AuthRequired | StartFailed | Broken
  -> Closing
  -> Closed

caller_admission: Free -> Claimed -> Free
session_turn_lease: Free -> CueOwned -> Free
                            CueOwned -> SupervisorOwned -> Free
```

- `Opening`：open request 已成功提交，当前 attempt 可能正在 preparation、spawn、
  initialize、new session、configuration 或 MCP readiness。
- `BackingOff`：最近一次 attempt 遇到 adapter 明确认定的 transient failure，已经完成
  旧资源清理，正在等下一次 retry；它对 Python 仍表现为 pending readiness。
- `Ready`：同一个 ACP session 和已发现 result tool 的 route 可以接受下一轮。
- `Active`：一个 Cue-owned operation 已经独占 session-turn lease；最多只有这一轮可以提交或
  处理 prompt。
- `Cancelling`：caller outcome 已确定，exact remote turn 已由 supervisor 接管并等待
  settlement；session 尚不可复用。
- `AuthRequired`：provider CLI 的预先登录状态缺失或已失效，且从未形成 Ready session；
  第一版是 Actor-lifetime terminal state。
- `StartFailed`：尚未形成可用 session，启动已确定为 deterministic failure；第一版是
  Actor-lifetime terminal state。
- `Broken`：session 曾经存在，但 Troupe 无法证明原 context 还能精确继续。
- `Closing/Closed`：Actor 已逻辑销毁，正在或已经完成物理清理。

caller admission 与 session-turn lease 是不同资源。第一个 `act()` 可以在 `Opening` 时取得
admission 并等待 shared readiness；取消 handoff 后，它释放 admission，但 supervisor 继续
持有 session-turn lease。下一次 `act()` 可以取得 admission 并等待 session availability，
却不能取得 lease 或发送 prompt；此时再来的调用仍然 fail-fast。

`SharedSessionAvailability` 是 generation-latched state barrier，不是 edge-triggered
notification。caller 在持有 admission 时，必须在同一原子状态检查中读取当前 generation
并注册 waiter；supervisor 以一次 transition 同时发布该 generation 的 `Ready/Broken/Closed`
结果、释放 session-turn lease 并唤醒 waiter，因此 settlement 与 subscribe 竞态不会丢 wakeup。

### 4.3 ACP open handshake

后台 open 至少执行：

1. 根据 immutable profile 解析并启动固定的 agent executable。
2. 建立 stdio ACP connection。
3. 发送 `initialize`，协商 stable protocol v1。
4. 保存 agent implementation info 和 capability snapshot，并要求
   `mcpCapabilities.http == true`；缺失或为 `false` 是
   `StartFailed(ProtocolIncompatible)`，不得继续创建 route 或发送 `session/new`。Troupe
   不消费 `authMethods`，也不发送 ACP `authenticate` request；认证必须由部署方在 cast 前
   通过 provider CLI 完成。
5. 只声明 Troupe 真实实现的 client capabilities；第一版不声明 ACP elicitation 或
   terminal/browser authentication capability，也不提供需要人类参与的 callback。
6. 等待 Production-owned `ResultMcpService` Ready。它以
   `TcpListener::bind(("127.0.0.1", 0))` 取得由操作系统保留的端口，从 live listener
   的 `local_addr()` 构造 URL；不得先 probe 一个端口、释放后再 bind。
7. 为每次 `session/new` invocation 分配新的 provisional Troupe session generation，并注册
   独立的随机 MCP server name、256-bit OS-CSPRNG bearer capability 和 route state。cast 时打开
   并持有 workspace directory handle；child 在 exec 前以
   `fchdir` 绑定该 handle，`session/new.cwd` 使用由同一 handle 支撑的 Linux
   `/proc/<owner-pid>/fd/<fd>` absolute alias。public identity 仍使用冻结的
   canonical path。由此 pathname 在检查后被同名替换也不能让任一 consumer 落到新目录。
   `session/new.mcpServers` 使用 ACP SDK 的 HTTP variant；token 只进入 exact
   `Authorization: Bearer ...` header，不进入 URL、公开 API 或持久化记录。
8. 发送携带该 provisional route 的 `session/new`。agent 可以在这个 request 返回前连接
   HTTP MCP；`ResultMcpService` 必须立即、独立地处理该 generation 的 initialize、initialized
   notification 和 tools/list，不能等待 provider session ID、configuration 或
   `session/new` response。MCP 成功只提交该 route 的 `McpReady` latch，不单独发布 Actor
   readiness。
9. `session/new` 明确返回 `auth_required` 时，无论 route 是否已经 `McpReady`，都先 revoke
   该 provisional generation、使其 latch 永久失效并拒绝其后续 request，随后提交 terminal
   `AuthRequired`。Troupe 不检查 credential、不选择 advertised method、不发送 `authenticate`、
   不打开 UI，也不在同一 process 内重试 `session/new`。
10. successful `session/new` 保存 provider session ID并通过 post-new workspace
    revalidation 后启动 configuration lane。按内置 `AgentLaunchSpec` 的 `ModeApplicationV1` 显式应用
    exact internal mode。若
   `session/new.configOptions` 提供 launch spec 固定的 mode option，调用一次
   `session/set_config_option`（即使 current value 已相等）并从其完整返回 snapshot 校验
   exact current value；否则只在 launch spec 固定 legacy mode ID、该 ID 存在于
   `session/new.modes.availableModes` 时调用一次 `session/set_mode`，以 correlated success
   response 作为 typed acceptance。Codex 还可在 spawn 前使用 launch spec 固定的
   `INITIAL_AGENT_MODE=agent`，但它不能替代上述 session-level apply/confirmation。缺少精确
   option/mode、返回 snapshot 非法、RPC 拒绝或确认值不一致都进入
   `StartFailed(ConfigurationInvalid)`；不能依赖 ambient/default value 恰好相同。
11. 从 mode apply 返回的完整 `configOptions` snapshot（legacy path 则使用
   `session/new` snapshot）中校验并设置 profile 请求的 model；使用该请求返回的完整新
   config snapshot，再校验和设置 effort。
   `effort is None` 时不发送 effort override，而是接受 model 设置后 agent
   报告的非 null 当前值。任一 present config option 的 `currentValue` 都必须符合其 ACP
   discriminated type；present null 永远是 protocol/configuration failure。只有 pinned
    adapter 明确允许整个 effort option 缺失时，internal effective effort 才可为 `None`。成功后
    提交同一 provisional generation 的 `ConfigurationReady` latch。
12. configuration lane 与第 8 步的 MCP lane 没有相对先后约束：`McpReady` 可以发生在
    `session/new` response 之前、configuration RPC 之间或 `ConfigurationReady` 之后；任一 lane
    都不得等待另一个 lane 才处理其 protocol traffic。Runtime 只要求对应 bearer route 最终依次
    完成 adapter-pinned exact MCP `initialize`、同 revision/tools capability/server identity response、
    `notifications/initialized` 和发现唯一 static result tool 的 `tools/list`。内置 adapter contract
    必须保证 MCP discovery 能在首个 user prompt 前完成，real acceptance suite 覆盖该行为。
13. `Ready` 是同一 provisional generation 上 successful `session/new`、post-new workspace
    revalidation、`ConfigurationReady` 与 `McpReady` 的一次性 join。join 时再次校验最终
    mode/model/effort current value，保存 requested/effective selection、provider `session_id` 和
    result-channel generation，再将该 generation 发布为 Actor 的正式 session generation。任何
    component 失败都 revoke route 并走 Opening failure；任何 component 尚未完成都保持
    `Opening`。首个 prompt 只能在 `Ready` commit 后发送。

ACP baseline 要求 agent 支持 `session/new`、`session/prompt`、
`session/cancel` 和 `session/update`。close、load、resume、
fork、list 等能力一律按 initialize capability 使用，第一版不把它们当最低要求。

readiness 是 shared single-assignment result。transient attempt failure 不提交它，
`BackingOff` 后仍由同一个 open operation 继续驱动。某个等待它的 Python task 被取消，
不会取消 Actor-owned startup；Actor 销毁才会取消 Opening/BackingOff 和相关资源。

### 4.4 Opening retry policy

`Ready` 之前，每个 attempt 都从 preparation 开始，到完整 configuration 与 MCP readiness
验证结束。
失败必须由 pinned adapter/preparation subsystem 分类为以下三类之一：

| 分类 | 例子 | 状态变化 |
|---|---|---|
| `Transient` | registry/DNS/连接返回明确临时错误、provider retry-after | 完整清理后进入 `BackingOff`，再自动 retry |
| `Authentication` | `session/new` 返回 `auth_required`，表示 provider CLI 的预先登录状态缺失或失效 | `AuthRequired` |
| `Deterministic` | package/version 不存在、protocol/MCP capability 不兼容、workspace/model/effort 无效、adapter contract violation | `StartFailed` |

unknown error 不得仅凭 stderr 文案认定为 transient。pre-initialize process crash、EOF
等模糊故障可以有限重试；相同 normalized failure fingerprint 连续达到内部 crash-loop
threshold 后必须提交 `StartFailed`。固定 threshold 见第 10.4 节。

每次完整 Opening attempt retry 前必须把旧 result route 标记为 closing 并 revoke capability，关闭 ACP
connection，立即终止并回收整个旧 guardian-owned process tree。`auth_required` 是 terminal authentication
failure，不进入 retry。pre-Ready cleanup 不等待
`session/close` response；该 attempt 从未对 Actor 发布 Ready，也没有接收任何 `act()`
prompt，因此没有需要保留的 conversation context。

所有已经被分类为可重试的 Opening failure 使用当前 build 固定的 `OpeningRetryBackoffV1`。
retry ordinal `r` 从 0 开始；窗口毫秒数为
`w = min(30_000, 250 * 2^min(r, 7))`，实际 delay 是闭区间
`[ceil(w / 2), w]` 上的均匀整数毫秒，由与 bearer generation 相同的 locked OS-CSPRNG
wrapper以 rejection sampling 取得。随机源失败是 deterministic
`StartFailed(PreparationFailed)`，不得退化成固定 delay 或弱随机。测试注入 random word 与
backoff wakeup，逐项证明窗口、30 秒 cap、unbiased range 和 ordinal；除已经确定 retry 后的
backoff pacing 外，production 不用 elapsed time 推进 lifecycle。

这个 delay 只在 Runtime 已经根据 typed event 决定“当前 attempt 失败且允许 retry”、完成
route revoke 与 whole-process cleanup 后，节流下一次 attempt；backoff wakeup 不发现 failure、
不改变 retry eligibility，也不为沉默的 operation 建立 deadline。明确的 `Transient` failure
可以这样持续重试到 Ready 或 Actor 销毁；ambiguous crash-loop只在尚未达到固定 threshold 时
使用同一节流规则。
package preparation、initialize、session/new、configuration 或 MCP readiness
只等待明确 response/error、EOF/process exit、Actor capability destruction 或 Production
shutdown；沉默可以让当前 attempt
永久停在 `Opening`。

### 4.5 Terminal startup state 与 reopen

`AuthRequired` 和 `StartFailed` 保存 immutable failure snapshot。正在等待 readiness
的调用以及所有后续 `act()` 都立即得到等价的 typed exception，不再触发新的 open
attempt。

第一版不提供 `authenticate()`、`retry_start()` 或 `reopen()` public API。transient
错误已经自动恢复；deterministic 错误在 immutable profile 下重试没有意义。认证必须由部署方
在 Troupe 外通过对应 CLI 于 cast 前完成；修复登录态后需要创建新的 Actor。未来若增加管理
API，只能显式定义它对 Actor identity 和 session identity 的影响，不能复用于 Ready 后的
context recovery。

本节只处理从未 Ready 的 startup。session 曾经 Ready 后发生的 auth failure、EOF 或
adapter crash 按第 7 节处理；无论最终 health 如何，都不能
回到本节通过 `session/new` 自动恢复。

## 5. 单次 `act()` 流程

### 5.1 同步 preflight

虽然用户看到的是 awaitable API，native 方法与 `ActorHandle.cue()` 一样先同步完成：

- authority 校验。
- `script` 类型、UTF-8 可编码性和输入上限校验。
- `output_schema` 深度遍历并编译为 immutable `CompiledActSchema`；校验 node 类型、
  placement、cycle、bounds 和 resource limits。内置 descriptor 被完整 snapshot；custom
  `SchemaValue` 的 base metadata 被 snapshot、`render_prompt()` 同步调用一次并冻结返回值，
  Runtime 保留 validator strong reference 供本轮使用。
- 构造只允许被驱动一次的私有 `_ActCall`。

这样非法上下文或非法 schema 在调用现场失败；从未 await、被 close 或被 GC 的
`_ActCall` 不会提交远端 turn。

### 5.2 Turn 状态机

```text
caller-owned _ActCall:
  Created
  -> Admitting
  -> WaitingSessionAvailable
  -> ClaimingSessionTurn
  -> ArmingResult
  -> SubmittingPrompt
  -> AwaitingResultAndSettlement
  -> Succeeded | Failed

  Admitting -> BusyRejected
  WaitingSessionAvailable | ClaimingSessionTurn | ArmingResult
    -> CallerCancelled
  SubmittingPrompt | AwaitingResultAndSettlement
    -> HandingOffToSupervisor
    -> CallerCancelled | Failed

supervisor-owned continuation:
  Adopted
    -> SettlingRemote
    -> SettledReady | SettledBroken | Destroyed
```

### 5.3 Admission 与执行

1. 向当前 `CuedScope` 登记结构化子操作。
2. 原子 claim Actor caller-admission token；失败时不创建 queue item。
3. 分配 Troupe operation ID。
4. 等待 shared session availability：`Opening/BackingOff` 等 startup readiness；
   `Cancelling` 等 supervisor settlement。等待期间仍持有 admission，所以不存在第二个 waiter
   或 FIFO。
5. 若 readiness 是 `AuthRequired`、`StartFailed`，或 session 进入 `Broken/Closing/Closed`，
   返回保存的 typed failure。
6. session 为 `Ready` 时，原子取得 session-turn lease、切换为 `Active`，再分配
   Actor-global monotonic turn index。只有取得 lease 的 operation 能发送 prompt。
7. 在 Actor result route 上 arm 本轮唯一的 `ResultSlot`，保存 operation ID、turn
   index、compiled act schema 和 output limits。只有 arm 成功后才能提交 provider prompt。
8. 构造 deterministic prompt envelope。exclusive ACP writer 在写入 request frame 第一字节
   前，先把 turn 原子标记为 `Submitted` 并固定 request correlation；这是 prompt submission
   线性化点。越过该点后 writer 必须继续完成同一 `session_id` 的 `session/prompt` frame，
   或报告明确 transport failure，不能被 caller future 的 drop 截断。
9. 持续处理与该 prompt request 相关的 `session/update`，不建立 per-turn event history；
   普通 update 在完成协议分类后直接丢弃。worker 对期间出现的 permission、plan review 和
   provider question 立即执行非交互响应。
10. 与 ACP prompt 并行等待 result MCP handler 原子接受一个 contract-valid value。
    validation failure 作为 tool execution error 返回给 agent，由同一 agent turn 自行
    修正；它不会结束或重放本轮。
11. 等 correlated prompt response 或 transport failure，并由 pinned adapter 归一化为
    authoritative terminal/error 或 uncertain settlement。
12. 根据 settlement 独立提交 session health；uncertain settlement 进入 `Broken`。
13. 只有 contract-valid result 已接受且 settlement 是 authoritative `end_turn` 时，才在
    未被取消的 operation 上提交 Python result。其他 terminal 组合映射为 typed failure。
14. 先把 exact arm generation 的 `ResultSlot` 转为 `Disarmed` tombstone，再从 route
    disarm；以一次 worker transition 发布 `Ready/Broken`、释放 session-turn
    lease 并完成当前 availability generation，注销 Cue operation，最后释放 caller
    admission。

result tool 调用与 ACP settlement 是两个独立条件。cancelled、refusal、limit、provider
failure 或 uncertain settlement 不能因为中途已经提交合法 value 而伪装成成功；反过来，
只有 `end_turn` 而没有合法 tool submission 也不能成功。

### 5.4 Cue drain

caller-owned `act()` 是 Cue 的结构化子操作；任何时刻 exact turn 都必须有且只有一个 owner：

- Cue 停止 active 后拒绝新 turn。
- Cue 返回、失败或取消时，必须取消仍登记的 caller-owned turn。
- prompt 尚未提交时，本地 unwind 后 Cue child 才能结束。
- prompt 已提交时，Cue child 只等待本地 supervisor handoff commit；commit 后 remote turn
  归 Actor supervisor 所有，Cue 可以 drain 并释放 Actor mailbox running slot。
- Cue/Scene/RunBinding 结束只要求 caller-owned phase 本地 unwind 或完成 supervisor handoff，
  不等待 handed-off remote settlement，也不关闭仍然存活的 Actor session。

这不是 ownerless detached work。supervisor 保留 session-turn lease，因此下一条 Cue 虽可
运行，其 `act()` 只能等待 session availability，不能与旧 turn 同时修改 conversation 或
workspace。

该 lease 只串行化 agent session turn，不是 Actor-wide filesystem lock。下一条 Cue 的普通
Python/外部代码仍可能接触同一 workspace，而被取消的 agent 在 authoritative settlement 前
也可能尚有 OS-side effect；第一版 trusted execution model 不对此作隔离承诺。

## 6. 内部 HTTP MCP result contract

### 6.1 Listener、端口与生命周期

V1 使用 Production-owned loopback HTTP MCP service，不使用 SQLite、
Unix socket 文件、每 Actor 独立 listener 或 stdio MCP bridge。

V1 的 MCP wire contract 固定为 adapter registry 选择的 stateful lifecycle revision 与
`McpTransportProfileV1` Streamable HTTP 子集。Runtime build 只包含一套实现该 profile 的
MCP server path；同一实现穷尽支持 registry 中出现的 `2025-06-18` 与 `2025-11-25`，但每条
route 只绑定其中一个 exact revision。ACP crate/version不能代替这个 MCP wire pin。Troupe
不接受`2026-07-28`取消initialize/session lifecycle后的stateless shape，也不做revision fallback。

service 首次需要时执行等价于：

```rust
let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
let address = listener.local_addr()?;
let endpoint = format!("http://127.0.0.1:{}/mcp", address.port());
```

这里 `0` 的含义是让操作系统在 bind 的原子操作中选择并占有一个可用 ephemeral port。
这就是端口搜索机制。禁止先扫描或 connect-probe 候选端口、关闭 probe socket、再让
HTTP server 重新 bind，因为中间窗口会产生 TOCTOU 竞态。也不能把 `localhost` 解析结果
当作地址，以免 provider 与 server 在 IPv4/IPv6 上不一致。

listener 必须在发布 endpoint 和发出任何 `session/new` 前进入 accept-ready，并一直由
native service 持有到所有 Actor session 关闭。服务意外丢失 listener 后，已经 Ready 的
session 仍指向旧 URL，不能通过换端口并静默 `session/new` 恢复；相关 Actor 必须停止新
turn、完成进程清理并进入 `Broken(ResultChannelLost)`。

`McpTransportProfileV1`精确规定：

- listener只接受HTTP/1.1；不协商HTTP/2/h2c或WebSocket/其他Upgrade。keep-alive可以复用，
  但同一connection必须串行处理request，在前一response完成前不解析下一组headers，也不创建
  第二个`ResultRequestLease`，所以每条live connection最多持有一份request lease；
- route第一条JSON-RPC message必须是MCP `initialize`，requested/negotiated revision都必须
  等于该 route 的 adapter pin；其后必须依次收到`notifications/initialized`和`tools/list`才算MCP-ready；
- client的每个POST `Accept`必须同时允许`application/json`和`text/event-stream`，JSON body
  使用正确Content-Type；initialize后的POST必须带与 route pin 相同的 exact
  `MCP-Protocol-Version`；
- server对JSON-RPC request只返回`application/json` response，不打开SSE stream；对合法
  notification返回空body HTTP 202；GET与DELETE返回405；
- Troupe不签发、不要求也不接受`MCP-Session-Id`，因为每个bearer route本身持有唯一
  provisional `session/new` invocation lifecycle state；
- `Origin`缺失可接受；存在时必须与endpoint使用相同loopback scheme与authority，否则在
  body业务处理和route mutation前拒绝。这个检查是固定transport conformance，不是sandbox承诺；
- wrong/missing/duplicate protocol header、乱序lifecycle、wrong revision、非法method/content
  negotiation和stateless first-call都fail closed，不切换另一套MCP解释器。

### 6.2 Session 注册、发现与路由

每次 `session/new` invocation 在发送 request 前创建一套 provisional generation：

- 一个带随机后缀的 MCP server name，避免与 provider 本地配置发生名称去重或覆盖；
- 一个高熵 bearer capability，映射到 Actor identity 和本次 provisional session generation；
- 共享 endpoint 上的独立 route state。

发送给 agent 的 ACP 配置概念上是：

```json
{
  "type": "http",
  "name": "troupe-result-<random-suffix>",
  "url": "http://127.0.0.1:<selected-port>/mcp",
  "headers": [{"name": "Authorization", "value": "Bearer <capability>"}]
}
```

具体 wire shape 由 ACP SDK 的 `McpServer::Http` typed variant 生成，不手拼 JSON；只有
initialize 明确 advertise HTTP MCP capability 才能构造它。bearer 是每次 `session/new`
invocation 独立生成的 32-byte OS-CSPRNG value，以 base64url-no-pad 编码。HTTP handler 在解析或修改
route/MCP state 前先验证 exact `Authorization` header，再用 capability 定位 Actor route；
missing/wrong header 不得触碰 route。URL 不包含 Actor ID 或 secret。`auth_required` 先 revoke
旧 provisional route 并关闭其 server-side connection，再提交 terminal `AuthRequired`；不会在同一
ACP process 内认证或创建新 invocation。完整 Opening attempt retry 还要 kill/reap 旧 agent process，
并为下一 attempt 的 `session/new` 生成全新 capability。Ready 后被 promote 的 capability 固定到该 Actor session，Actor
capability destruction 时 revoke。

header 验证与 route lookup 成功后、读取或 decode 任意 body byte 前，handler 必须从 route
原子取得 immutable `ResultRequestLease`。它固定本次 session generation、route revocation
cell，以及请求到达时的 `Option<(ArmGeneration, Arc<ResultSlot>)>`。后续 MCP
lifecycle/tool dispatch、schema validation 和 acceptance 只能使用这份 snapshot，
不得在 body 完成后重新读取 route 当前 slot。disarm 先把旧 slot 原子 tombstone，再允许下一轮
arm；因此一个跨轮停留的 partial request 最多持有旧 slot，永远不能提交到新 slot。route revoke
也会使已取得 lease 的后续处理确定失败。最终 result CAS 必须与 route revoke/disarm 在同一
epoch state cell上线性化：revoke/disarm先赢则旧slot拒绝，旧slot acceptance先赢则随后cleanup
丢弃该旧value；两种顺序都不能触碰新arm。lease 只活到 request/connection 结束；永久 partial
request 可以永久持有旧的有界 slot，但同时占用一个 Production-global live-connection permit，
所以这类引用总数仍由 65,536 上限约束，不需要 timeout 或 per-Actor quota。

bearer capability 是内部路由与防误投机制，不是 sandbox。第一版 trusted execution
model 下，同一 OS identity 内的恶意代码仍不属于 Troupe 能隔离的威胁模型。

`AgentSessionSlot` 只有在 result service 已 Ready、同generation的`session/new`已成功、
`ConfigurationReady`已提交，并且对应 route 按上述exact lifecycle观察到 initialize、initialized 与包含
result tool的成功tools/list后才进入`Ready`；configuration与MCP discovery没有相对顺序。
adapter 是否会在首个 prompt 前建立 MCP connection 必须由 real acceptance test 覆盖；
第一版不通过隐藏 bootstrap prompt 污染 Actor 上下文。

### 6.3 静态 result tool

所有 Actor session 暴露同一个逻辑工具名 `troupe_submit_result`。其公开 input schema
保持静态，从而不依赖三个 provider 对 `notifications/tools/list_changed` 的实现一致性：

```json
{
  "type": "object",
  "properties": {"value": {"type": "object"}},
  "required": ["value"],
  "additionalProperties": false
}
```

这是 MCP 协议要求的固定 tool `inputSchema`，不是 public `output_schema`，也不意味着
Troupe 对调用方暴露 JSON Schema。`Actor.act()` 的顶层 result contract 固定为 dict，因此
`value` 也明确声明为 JSON object，不能传 JSON-encoded string；MCP server 仍须校验这个
base schema，client-side tool schema 不能替代 server validation。

public `output_schema` 不动态改写 MCP tool definition。preflight 得到的
`CompiledActSchema` 同时存入本轮 `ResultSlot` 并由 prompt renderer 生成一个明确标为
placeholder 的 JSON-like shape，例如：

```text
Submit one result object through troupe_submit_result.
Extra fields are forbidden.

{
  "decision": <string; 审查结论; one of ["approve", "request_changes"]>,
  "score": <int64; 结果质量评分; inclusive range 0..100>,
  "metadata": {
    "note"?: <string or null; 补充说明>
  }
}

`?` marks an optional field. Angle-bracket text is a constraint, not a literal value.
```

prompt 固定要求成功提交一个 accepted result；validation error 时在同一 turn 修正后
重试，accepted 后不得再次调用。assistant 最终文本不是返回通道，且没有人类会回答澄清
或审批。exact script 与 rendered contract 必须作为不同结构段插入版本化 template，不能
用可被内容破坏的 delimiter 拼接。内置 node 的 renderer 只读取 compiled IR；custom node
使用 preflight 已经调用并冻结的 `render_prompt()` fragment，HTTP handler 不重新 render。
Runtime 始终负责 field path、required marker、native JSON kind、description 和结构 framing，
custom fragment 只描述该 value 的附加业务约束。

### 6.4 Typed schema compilation 与 `ResultSlot`

#### 6.4.1 Public descriptors

`troupe.act_schema.SchemaValue` 是 public、abstract、可直接或间接继承的基类。内置 concrete
Value 都派生自它并在 type stub 标记为 `final`；受支持的自定义入口是继承
`SchemaValue`，而不是继承某个 concrete native Value。概念 API 为：

```python
JsonKind = Literal["string", "int64", "float64", "bool", "array", "object"]
SchemaValueT = TypeVar("SchemaValueT")

class SchemaValue(Generic[SchemaValueT], ABC):
    description: str
    json_kind: JsonKind

    def __init__(self, *, description: str, json_kind: JsonKind) -> None: ...

    @abstractmethod
    def render_prompt(self) -> str: ...

    @abstractmethod
    def validate(
        self,
        value: SchemaValueT,
        /,
    ) -> None | Awaitable[None]: ...

class ValueRejected(ValueError):
    def __init__(self, message: str) -> None: ...

class SchemaCallbackError(RuntimeError):
    phase: Literal["render_prompt", "validate"]
    path: str
```

`SchemaValue`、`ValueRejected`、`SchemaCallbackError` 与全部 built-ins 从
`troupe.act_schema` 暴露。built-in 的 Python `render_prompt()`/`validate()` 与 native
contract 语义一致，但 Runtime 对 exact built-in type 直接编译，不通过 Python callback。
custom subclass 必须调用 base constructor，提供非空 description 和单一 native-prechecked
`json_kind`，并实现两个方法；没有 generic `AnyValue`，nullability 仍通过
`NullableValue(custom_value)` 表达。

`SchemaValue[T]` 的 `T` 是 Python 静态类型契约；type checker 用它检查 `validate()` override 和
调用方推断。Runtime acceptance 仍以 `json_kind` 为准，固定映射为 `string -> str`、
`int64 -> int`、`float64 -> float`、`bool -> bool`、`array -> list[JsonValue]`、
`object -> dict[str, JsonValue]`。扩展作者必须让声明的 `T` 与该映射一致；Runtime 不读取
annotation，普通 type checker 也不能从动态 constructor call 证明这层对应关系。错误 annotation
不会扩大 native-prechecked kind，callback 收到的永远是 `json_kind` 对应的 Python value。

第一版 built-in 集合为：

| descriptor | accepted MCP JSON | constraints | Python value |
|---|---|---|---|
| `StrValue` / `SchemaValue[str]` | string | required `description`；`min_length`/`max_length`；optional typed `choices` | `str` |
| `Int64Value` / `SchemaValue[int]` | 无 fraction/exponent 的 signed 64-bit integer token | required `description`；`min`/`max`；optional typed `choices` | `int` |
| `Float64Value` / `SchemaValue[float]` | 能转换为 finite IEEE-754 binary64 的 JSON number；integer token 也允许 | required `description`；finite `min`/`max`；optional typed `choices` | `float` |
| `BoolValue` / `SchemaValue[bool]` | JSON boolean；绝不把 boolean 当 integer | required `description`；optional typed `choices` | `bool` |
| `ListValue(item, ...)` | array | required `description`；`min_items`/`max_items`；item 是 `SchemaValue` | `list` |
| `ObjectValue(fields=..., ...)` | object | required `description`；fixed `FieldSpec` fields；extra fields forbidden | `dict` |
| `NullableValue(inner)` | `null` 或 inner value | 是 `SchemaValue` wrapper，并继承 inner description；只改变 nullability | `None` 或 inner Python value |
| `Field(inner, required=...)` | 由 inner 决定 | 不是 `SchemaValue`；只允许在 object field position；显式控制 presence | missing 或 inner Python value |

对应的 public constructor shape 为：

```python
@final
class StrValue(SchemaValue[str]):
    def __init__(
        self,
        *,
        description: str,
        min_length: int | None = None,
        max_length: int | None = None,
        choices: Sequence[str] | None = None,
    ) -> None: ...

@final
class Int64Value(SchemaValue[int]):
    def __init__(
        self,
        *,
        description: str,
        min: int | None = None,
        max: int | None = None,
        choices: Sequence[int] | None = None,
    ) -> None: ...

@final
class Float64Value(SchemaValue[float]):
    def __init__(
        self,
        *,
        description: str,
        min: float | None = None,
        max: float | None = None,
        choices: Sequence[float] | None = None,
    ) -> None: ...

@final
class BoolValue(SchemaValue[bool]):
    def __init__(
        self,
        *,
        description: str,
        choices: Sequence[bool] | None = None,
    ) -> None: ...

ItemT = TypeVar("ItemT")

@final
class ListValue(SchemaValue[list[ItemT]], Generic[ItemT]):
    def __init__(
        self,
        item: SchemaValue[ItemT],
        *,
        description: str,
        min_items: int | None = None,
        max_items: int | None = None,
    ) -> None: ...

@final
class ObjectValue(SchemaValue[dict[str, JsonValue]]):
    def __init__(
        self,
        *,
        description: str,
        fields: dict[str, FieldSpec],
    ) -> None: ...

ValueT = TypeVar("ValueT")

@final
class NullableValue(SchemaValue[ValueT | None], Generic[ValueT]):
    def __init__(self, inner: SchemaValue[ValueT]) -> None: ...

@final
class Field(Generic[ValueT]):
    def __init__(self, inner: SchemaValue[ValueT], *, required: bool) -> None: ...
```

`FieldSpec` 的 public typing 概念为 `SchemaValue[Any] | Field[Any]`。root 必须是 exact Python
`dict[str, FieldSpec]`，因此 root 本身没有 description；nested object 不再接受裸 `dict`，
必须使用 `ObjectValue`。bare `SchemaValue` field 默认 required。`Field` 缺失时不注入
default；`Field(NullableValue(...), required=False)` 明确区分 missing、present-null 和
present-value。

每个 description 必须是至少包含一个非空白 Unicode scalar 的 exact `str`。Runtime 保留原文，
不 trim 或 normalize；长度受第 10.4 节约束。内置 descriptor 与 `Field` wrapper 都是
immutable、可复用 object。constructor 立即 snapshot 所有 mutable input，包括 scalar choices 和
`ObjectValue.fields` mapping；field declaration order 取构造调用时的 mapping iteration order。
调用方随后修改原 sequence/mapping 不影响 schema node 或 compiled contract。

scalar `choices` 是真正的 acceptance constraint，不只是 prompt hint：

- `StrValue`、`Int64Value`、`Float64Value`、`BoolValue` 分别只接受本类型的 finite sequence；
  bare string 不能冒充 string choices sequence，boolean 不能冒充 integer/float choice；
- sequence 必须非空，按声明顺序 snapshot；native canonicalization 后的重复值同步拒绝；
- 每个 choice 必须先满足该 descriptor 的 type、length/range 与 finite 约束，不允许构造内部
  自相矛盾的 contract；
- `Float64Value` choice canonicalize 为 finite binary64；合法 integer JSON token 物化为 float 后
  可以匹配对应 choice；
- validation 使用 strict typed equality；错误显示 declaration-ordered allowed values，并产生
  `AgentResultIssue(code="not_in_choices")`；
- 单元素 choices 自然表达 constant；`NullableValue` 的 `null` 不放入 choices，而由 wrapper 控制。

所有 built-in validation 都 strict、无 coercion：string 不转 number，integer 不转 boolean，
`1.0`/`1e0` 不满足 `Int64Value`，而整数 `1` 可以满足 `Float64Value` 并物化成 `1.0`。
constraint constructor 拒绝 boolean-as-number、NaN/infinity、负长度、越界 bound 和
`min > max`。

descriptor constructor 校验自己的局部参数；`act()` preflight 再遍历整张 graph，拒绝
cycle、非法 node/placement、concrete built-in subclass 和违反第 10.4 节
`ResourceLimitsV1` 的 graph。built-in graph 被完整 snapshot。custom object 不 deep-copy：
preflight snapshot 其 base metadata；对每个 distinct custom object identity 在本次 `act()` 中
同步调用一次 `render_prompt()` 并冻结返回 fragment，同一 object 出现在多个 schema path 时复用
该 fragment，同时为本轮保留 custom object strong reference。render failure 的 `path` 使用
declaration order 的第一个 occurrence。之后对象或外部数据库状态发生变化可以影响
`validate()`，但不能改变已发送 prompt；两者一致性、callback reentrancy 和外部状态策略由
扩展作者负责。

custom validation 示例：

```python
class MultiRangeIntValue(act_schema.SchemaValue[int]):
    def __init__(
        self,
        *,
        description: str,
        ranges: tuple[tuple[int, int], ...],
    ) -> None:
        super().__init__(description=description, json_kind="int64")
        self.ranges = ranges

    def render_prompt(self) -> str:
        ranges = " or ".join(f"[{low}, {high}]" for low, high in self.ranges)
        return f"must be in one of these inclusive ranges: {ranges}"

    def validate(self, value: int) -> None:
        if not any(low <= value <= high for low, high in self.ranges):
            raise act_schema.ValueRejected("value is outside every allowed range")
```

`render_prompt()` 必须同步返回 bounded、非空白 exact `str`。Runtime 在 preflight 只调用一次，
并负责 surrounding path、required marker、json kind、description 和 escaping。callback exception、
非法 return type 或超限 fragment 同步包装为
`SchemaCallbackError(phase="render_prompt", path=<field-path>)`，原异常保存在
`__cause__`；prompt 尚未提交，session 不变。

`validate()` 可以同步返回 `None`，也可以返回一个最终 resolve 为 `None` 的 awaitable；I/O 场景
应使用 async callback。Runtime 不把 sync callback 隐式移到 thread pool，它在 active Python
loop 中直接执行；Troupe 不能抢占正在运行的 sync Python function，阻塞 loop 是扩展作者可见的
责任。async callback 在独立 tracked task 中 await，因此 bridge close 可以请求 task
cancellation。callback 收到 native tree 对相应 subtree生成的 defensive Python JSON copy；修改该
copy 或返回其他对象不能变换 authoritative value。
只有返回/resolve `None` 表示成功：

- 抛 `ValueRejected(message)` 表示 agent value 不满足 custom contract。Runtime 使用当前 schema
  path 和固定 `custom_validation` issue code 返回 MCP correction，计入一次 invalid call；
- 任意其他 exception、非 `None` return 或 awaitable protocol failure 包装为
  `SchemaCallbackError(phase="validate", path=...)`。它是调用方 schema code failure，不是
  agent validation error，不计 invalid-call budget，也不把原异常文本发给 agent；
- async callback 收到 bridge/caller 已接受的 task cancellation 后抛出的
  `asyncio.CancelledError` 是 cancellation outcome；bridge 仍 open 时由 callback 自行抛出的
  `CancelledError` 是 `SchemaCallbackError`，不能伪装成 caller cancellation；
- 同一 custom validator 可能因 list item、多个 MCP submission 或多个 Actor 被反复/并发调用。
  Runtime 只保证同一个 `ResultSlot` 内串行；扩展必须可重入、幂等、支持 cancellation，且不得
  假定一次 callback 对应一次最终 caller success；
- Runtime 不为 callback 设置 timeout。callback 可以自行使用数据库、网络或 Python timeout；
  永久 pending callback 会使本次 validation pending，和其他用户 Python code 一样不受 Troupe
  sandbox、purity 或 termination guarantee。

custom validation 是受信任的业务代码 escape hatch，不把 `output_schema` 变成 security
boundary。第一版仍不提供 public JSON Schema、general union、dynamic-key built-in map、tuple、
recursive reference、default injection 或 extra-field opt-out；这些语义可以由 custom validator
自行检查，但 Troupe 不为其生成 native constraint 或静态类型推断。

#### 6.4.2 Single compiled IR

概念类型为：

```rust
struct CompiledActSchema {
    root: ObjectContract,
    prompt_template_version: PromptTemplateVersion,
    validation_mode: SchemaValidationMode,
    custom_validators: Vec<CustomValidatorBinding>,
}

enum SchemaValidationMode {
    NativeOnly,
    Hybrid,
}

enum ValueContract {
    String {
        description: String,
        min_length: Option<u64>,
        max_length: Option<u64>,
        choices: Option<Vec<String>>,
    },
    Int64 {
        description: String,
        min: Option<i64>,
        max: Option<i64>,
        choices: Option<Vec<i64>>,
    },
    Float64 {
        description: String,
        min: Option<f64>,
        max: Option<f64>,
        choices: Option<Vec<f64>>,
    },
    Bool {
        description: String,
        choices: Option<Vec<bool>>,
    },
    List {
        description: String,
        item: Box<ValueContract>,
        min_items: Option<u64>,
        max_items: Option<u64>,
    },
    Object {
        description: String,
        contract: ObjectContract,
    },
    Nullable(Box<ValueContract>),
    Custom {
        description: String,
        json_kind: JsonKind,
        prompt_fragment: String,
        validator_id: CustomValidatorId,
    },
}

struct ObjectContract {
    fields_in_declaration_order: Vec<FieldContract>,
    // lookup index omitted; extra fields are always rejected in v1
}

struct FieldContract {
    name: String,
    required: bool,
    value: ValueContract,
}

enum ValidatedActValue {
    String(String),
    Int64(i64),
    Float64(f64),
    Bool(bool),
    Null,
    List(Vec<ValidatedActValue>),
    Object(Vec<(String, ValidatedActValue)>),
}

struct CustomValidatorBinding {
    validator: Py<PyAny>,
}
```

prompt renderer、native validator、custom validation walker 和 PyO3 materializer 共享这份
compiled contract。`NativeOnly` 不创建 Python bridge，也不调用任何 Python method；其行为与原
closed DSL fast path 相同。`Hybrid` 的 native validator 先产出 `ValidatedActValue` 与
declaration/depth-first ordered `CustomValidationJob` 列表，不在 callback 后重新解释 generic
JSON。object 按 schema declaration order 产出，optional missing field 省略；array 保留 agent
提交顺序。compile-time custom binding 按 Python object identity memoize，job 仍按每个
schema/value occurrence 产生；custom callback 只决定 job 对应 subtree 是否被接受，不能替换
native value。

#### 6.4.3 Tool validation 与 acceptance

同一 Actor 只有一个 in-flight turn，因此 agent 不提交 operation ID；Runtime 以已认证的
Actor route 定位当前 slot。slot 在 `session/prompt` 前 arm，并在 operation 终态后
disarm。没有 active slot 的调用返回 tool execution error，绝不能缓存给下一轮。

内部状态概念上是：

```rust
struct ActiveResultContract {
    session_generation: SessionGeneration,
    arm_generation: ArmGeneration,
    operation_id: OperationId,
    turn_index: u64,
    schema: Option<Arc<CompiledActSchema>>,
    custom_validation: Option<PythonSchemaValidationBridgeLease>,
    invalid_calls: u8,
    state: ResultState,
}

enum ResultState {
    Awaiting,
    Accepted(ValidatedActValue),
    Rejected(ResultContractFailure),
    Settling(ResultTombstone),
    Disarmed(ResultTombstone),
}
```

`Settling` 表示 caller outcome 已确定但 remote turn 尚未 settlement。handoff 将
`Awaiting/Accepted/Rejected` 原子转成这个 tombstone；若此前已有 accepted value，立即丢弃
其 content，只留下有界 validation/size evidence。进入 `Accepted`、`Rejected` 或 `Settling`
时都关闭 bridge，并从 slot 取走/drop compiled schema 与 custom validator handles；这些状态不再
需要执行 validation。已经开始的 callback 只由独立 Python task lease 持有所需 reference，直到
completion 或 loop teardown。这样永久 `Settling` 不会永久保留用户 schema/database client。
slot 仍 arm 到原 operation，以便准确拒绝 late tool call，绝不能让它落入下一轮。

所有 operation 终态的 disarm 都先把该 generation 的 slot 原子转成 `Disarmed` tombstone，
再从 route 清除 current slot 并允许下一 arm。已在 header 后取得的 `ResultRequestLease`
保留旧 `Arc`；它完成 body decode 后只能看到旧 slot 的 `Settling`/`Disarmed`/其他旧状态，
不能解析到 route 中后来安装的 generation。

MCP/JSON-RPC decoder 先保证 request 是合法 JSON 和合法 tool-call envelope。handler 再按
compiled contract 执行：

1. 使用 request lease 检查 slot 仍是 exact generation 的 `Awaiting`。
2. `NativeOnly` 不取得 validation mutex。`Hybrid` 先取得该 slot 的单一
   `CustomValidationPermit`；同 slot 的其他 hybrid request 等待该 permit，并在取得后重新检查
   slot/route/bridge generation。permit acquisition 必须同时等待 bridge/slot close signal；handoff
   关闭 bridge 后，queued request 不再等前一个 callback，而是立即返回 `turn is settling`。
   这个内部 HTTP submission serialization 不是 Actor `act()` FIFO。
3. 执行全部 native byte/node/depth、JSON-kind、built-in structure/type/range/choices validation。
   若存在 native issue，完全不调用 custom callback；Hybrid permit 在发布该次 invalid outcome 后释放。
4. custom jobs 按 declaration/depth-first path order 逐个调度到 caller-owned
   `PythonSchemaValidationBridge`，在每次 dispatch 前和每次 sync/awaitable completion 后重新检查
   bridge open、route epoch、arm generation 与 slot state。native handler 等待 callback completion
   时也同时等待 bridge close；close 先发生就立即释放 handler并忽略 task 的晚到 outcome，不 join
   Python task。第一个 rejection/fault 后不再运行后续 callback。
5. 全部成功后，用最初 native `ValidatedActValue` 执行 existing first-valid-wins CAS，再释放
   Hybrid permit。

具体 outcome 为：

- invalid-call budget 尚未耗尽时，validation error 不改变 slot，返回 `isError: true`，并
  提供有界 JSON Pointer path、expected constraint 和 actual kind，例如
  `/int_field: expected int64 in inclusive range [0, 100], got string`；
- `ValueRejected` 走相同 correction path，issue code 固定为 `custom_validation`，message 使用
  bounded 用户文本；它不暴露 callback traceback；
- 前 8 次 validation failure 增加 `invalid_calls` 并保持 `Awaiting`；第 9 次以
  `TooManyInvalidCalls` 原子进入 `Rejected`，随后把 caller typed failure 与 remote
  settlement 分离，并 handoff 给 supervisor 取消；
- `SchemaCallbackError` 不增加 `invalid_calls`。它与 caller cancellation 在同一个
  `LinearizedTurnControl` 上竞争：callback fault 先赢时，Runtime 关闭 bridge、把 slot 转为
  `Settling`、以 `SupervisorOwned(Failed)` handoff remote turn 并让 caller 得到
  `SchemaCallbackError`；cancellation 先赢时 caller 仍只得到 `CancelledError`，late callback fault
  不再交付给 caller，也不保留。对应 MCP request 在 route 仍可响应时只得到 generic bounded
  `schema validation callback failed` tool error，不能看到 user exception text 或 traceback；
- 第一个合法 value 先完整构造成 `ValidatedActValue`，再以 compare-and-set 原子进入
  `Accepted`，同 transition 关闭 bridge并释放 schema/custom handles，然后才向 MCP client 返回
  success；
- `Accepted` CAS 与 supervisor handoff 的 `Settling` CAS 竞争。handoff 先发生时，当前及
  后续 tool call 都返回确定的 `turn is settling` execution error，不能再提交 caller result；
- `Accepted` 后任何调用都返回 `already submitted`；`Rejected` 后任何调用都返回
  terminal result-contract error。两者都不能被后续 value 覆盖。

Troupe 不追加 repair prompt，也不重放原始 script。修正只发生在当前 agent turn 内部，
由 MCP tool error 驱动；最大 schema depth/field count、请求体、string/list/object size、
description/choices/custom prompt/rejection detail、validation error 数、invalid-call 数和 turn
limits 统一由第 10.4 节固定。

`NativeOnly` 的 PyO3 仍只在 accepted value 与 authoritative `end_turn` 都满足后进入。
`Hybrid` 为 custom callback 在 result acceptance 前额外创建 defensive subtree copy；最终 caller
result 仍只递归物化 authoritative `ValidatedActValue`，绝不复用或信任 callback 收到的 mutable
Python object。在已定义的数据域内最终 conversion 是 total content conversion；剩余失败只可能是
interpreter shutdown、allocation 等 runtime failure，不能归类成 agent validation error，也不应
让 agent 重做已经结束的 turn。

### 6.5 成功边界与事件处理

Python result 的必要且充分条件是：

1. 当前 `ResultSlot` 已原子接受一个 `ValidatedActValue`；
2. 原 `session/prompt` 得到 adapter-authoritative `end_turn`；
3. 在最终 commit 线性化点前，operation 没有接受 cancel。

Runtime 不保存 assistant update/final text、tool trace 或 MCP request 的 per-turn history。
worker 完整读取、decode 和 classify 每条 ACP update，只把 result submission、correlated
response、stop reason、permission/auth request、EOF/transport failure 和 settlement evidence
中 correctness 所需的部分保留在有界 control state。assistant text chunk、plan/status、usage、
tool progress/output 等不参与 correctness 的 payload 在处理后直接释放。任何文本都不被扫描、
拼接、剥 Markdown fence 或作为 fallback 解析。

authoritative `end_turn` 时 slot 仍为空，本轮以 typed `AgentResultMissingError` 失败；只有 invalid
submissions 时可以在 detail 中附带 `ResourceLimitsV1` 约束的 validation 摘要。已经接受
value 但随后出现 refusal、limit、cancel、authoritative error 或 uncertain settlement，
本轮仍按该 terminal 失败；Runtime 丢弃 value content，只保留完成 typed failure 与 terminal
transition 所需的常量大小 correctness state。

第一版 ResultSlot 只存在于 Runtime 内存。SQLite 或让 agent 直接写 Runtime 数据库既不
减少协议校验，也会扩大 process、并发和事务边界，因此不是结果通道。未来若支持
process crash 后恢复，必须为 Actor session、turn settlement 和 result commit 一起设计
durable journal，不能只持久化 agent 写入的 value。

## 7. Turn settlement、取消与竞态

普通 cancellation 可能来自 caller task、CuedScope、Scene 或 RunBinding teardown；Actor
capability destruction 和 Production shutdown 则是可以抢先终止 settlement 的 destructive
lifecycle event。

### 7.1 Authoritative settlement

收到一条看起来像 terminal 的消息还不够。pinned adapter 必须证明原
`session/prompt` 已经唯一终止、所有属于该 turn 的合法尾部 update 已经先于 response
交付，并且没有未决 reverse request 或无法归属的 provider activity。

内部归一化结果为：

```rust
enum TurnSettlement {
    NotSubmitted,
    AuthoritativeTerminal { stop_reason: AcpStopReason },
    AuthoritativeError { error: NormalizedAgentError },
    Uncertain { reason: SettlementFailure },
}
```

`AuthoritativeError` 只适用于 adapter conformance 明确证明底层 turn 已经结束、connection
仍健康的 request error；任意未知/malformed error 都是 `Uncertain`。判定表固定为：

| observation | 当前 `act()` | session health |
|---|---|---|
| prompt 未发送 | 本地取消/失败 | `Ready`，或仍保持 `Opening` |
| authoritative `end_turn` + accepted result | 提交已校验 result | `Ready` |
| authoritative `end_turn` + 没有 result tool attempt | `AgentResultMissingError` | `Ready` |
| authoritative `max_tokens` / `max_turn_requests` / `refusal` | typed turn failure | `Ready` |
| adapter-authoritative `cancelled` | 已 handoff 时 caller 已得到 `CancelledError`，supervisor 只完成后台 settlement；否则为 typed turn failure | `Ready` |
| adapter-authoritative request error | typed turn failure | `Ready` |
| invalid result tool calls 后仍没有 accepted result | typed result failure，附有界 validation detail | `Ready` |
| accepted result 后出现非-`end_turn` authoritative terminal | 该 terminal 的 typed turn failure，丢弃 result content | `Ready` |
| EOF、connection loss、adapter/process crash | typed turn failure | `Broken` |
| unknown error、malformed response、terminal 后非法 late update、仍有 unresolved request | typed turn failure | `Broken` |
| post-Ready `auth_required` | `AgentSessionBrokenError(code="authentication_lost")` | `Broken(AuthenticationLost)` |
| post-Ready mode update/config snapshot 改变 frozen internal mode/model/effort，或 known option shape/current value 非法 | commit 前为 `AgentSessionBrokenError(code="protocol_violation")`；commit 后不撤销已返回值 | `Broken(ProtocolViolation)` |

turn outcome 与 session health 是正交结果。若 authoritative response 和 Python result
commit 已经线性化，随后发生的 idle EOF 不反向撤销本轮成功；Actor 同时进入 `Broken`，
后续 `act()` 失败。若 EOF 抢在 commit 前发生，即使 slot 已接受 value，也不能再证明
session 可复用，本轮按 uncertain failure 结束。

ACP 可以在 Ready 后随时发送完整 `config_option_update` snapshot或legacy
`current_mode_update`。Runtime 必须逐份验证 `AgentLaunchSpec`-fixed mode application 以及 pinned
model/effort option ID、type、domain 和 current value：mode始终保持internal exact value，model 始终保持 requested exact
value；显式 effort 保持 requested exact value；`effort=None` 保持 Ready 时冻结的有效
effective value或允许的 option-absent 状态。其他合法非mode option可以变化。mode/selection drift 与
result success commit 在同一 session-state critical section 竞争：drift 先提交时当前未提交
turn 失败并进入 `Broken`；result commit 先提交时本轮已验证 `dict` 保持成功，随后进入
`Broken` 并拒绝后续 turn。Runtime不发送mode reapply或其他reconfigure；这里没有按时间判断
drift，也不自动 fallback。

同一次child termination通常同时产生process-exit与stdio EOF。每个Opening attempt或
post-Ready session因此持有一个`TerminalFaultLatchV1`：worker先把已观察事件完整分类为
`process_exited`、`transport_lost`、`result_channel_lost`或`protocol_violation`，再对latch执行
单一CAS。第一个线性化的fault冻结public error snapshot、当前caller/availability outcome和
唯一cleanup lease；同generation的后续fault只作为同一次cleanup的bounded corroboration，
不能覆盖code、再次完成caller、再次revoke route、增加Opening retry/fingerprint count或再次
kill/reap。这里没有debounce、优先级等待或timer：process exit先CAS就得到
`process_exited`，EOF先CAS就得到`transport_lost`，两种结果都合法但每次只能有一个winner。

Opening、idle Ready、Active和Cancelling使用同一latch。Opening中的loser不形成第二个attempt
failure；Active中的winner与result-success commit按既有临界区竞争；Cancelling中的fault不能
改写caller已经收到的`CancelledError`，只冻结session与later waiter看到的Broken snapshot。
deterministic tests必须在四种状态分别覆盖process->EOF与EOF->process两种barrier顺序，并逐项
证明一个outcome、一次route revoke、一个cleanup lease和一次whole-process-group reap。

### 7.2 已提交前

turn 尚未越过 prompt submission 线性化点时：

- 取消本地 operation。
- 若已经 arm 但尚未发送 prompt，disarm `ResultSlot`。
- 注销 Cue child operation。
- 若已取得 session-turn lease，则把 session 从 `Active` 恢复为 `Ready` 并释放 lease。
- 释放 caller admission。
- 不取消 Actor-owned startup。
- 若等待的是上一轮 supervisor settlement，不取消或修改该 supervisor-owned turn。
- 不改变 session context。

这里不需要 supervisor handoff，因为 writer 尚未开始暴露 request frame，不存在可能被
provider 执行的 remote turn。

### 7.3 已提交后

turn 已越过 prompt submission 线性化点时，即使 request frame 仍在 writer 中：

1. 在 operation 上只接受一次 cancel；重复 cancel 不创建第二个 control operation。
2. 执行一个无网络 I/O、无有界 channel enqueue、无需在取消现场分配内存的本地原子 handoff。
   turn 创建时已预留的 supervisor continuation 接走 exact prompt correlation、session-turn
   lease、armed `ResultSlot`、reverse-request state 和 settlement evidence；同一个 transition
   先把 `ResultSlot` 置为 `Settling` 并关闭 `PythonSchemaValidationBridge`，再让 session 从
   `Active` 进入 `Cancelling`。bridge close 使 queued/new callback dispatch 立即失败，并冻结
   in-flight callback 的 cancellation handle。
3. 只有 handoff commit 后，注销 Cue child operation、释放 caller admission，并让
   `_ActCall` 抛 `asyncio.CancelledError`。这些动作不等待 provider response、ACP writer
   flush、process exit 或 async custom callback cooperative exit；Runtime 对 in-flight async
   Python task 发出 cancellation request，但不 join 它。正在 Python loop 内执行的 sync callback
   不能被抢占；若它阻塞 loop，Runtime 只能在其交还控制权后观察 cancellation、提交 handoff 并
   返回 caller，这与任意同步 Python user code 的 cancellation boundary 相同。
4. supervisor 将尚未完成的 turn-scoped tool call 标记为 cancelling，对所有 pending
   `session/request_permission` 返回 ACP `cancelled` outcome，并在尚未观察到 authoritative
   terminal 时确保向 exact `session_id` 发送且只发送一次 `session/cancel` notification。
   `ResultSlot` 已是 `Settling` tombstone，继续归属并拒绝 late result call。supervisor 不执行、
   重试或等待用户 `validate()`；custom callback 不是 remote settlement evidence。
5. supervisor 继续驱动尚未完成的 prompt frame write，接收和归属该 turn 的合法尾部
   updates 与 reverse requests，不建立 per-turn history；然后在无 clock 条件下等待
   correlated response、明确 connection/process failure，或 Actor capability
   destruction/Production shutdown。
6. authoritative settlement 到达时，supervisor disarm 并丢弃任何不再有 caller 的 result，
   把 session 发布为 `Ready`，释放 session-turn lease 并唤醒 availability waiter。明确或
   uncertain failure 则先 disarm slot、发布 `Broken`，再释放 lease、唤醒 waiter 并执行既定
   cleanup；销毁则发布 `Closed`。

不能只取消本地 future 而不转移 ownership。也不能在 handoff 时释放 session-turn lease，
否则下一轮可能与旧 turn 同时操作同一 context。caller 能立即结束，是因为 lease 仍由
supervisor 明确持有，而不是因为 Runtime 假定远端已经停止。

被请求取消的 async Python callback task 由 bridge 的 RunBinding task lease 持有到它结束或
Python loop teardown；sync callback 返回后的 outcome 也必须重新检查 bridge generation。两者的
return、`ValueRejected` 或 exception 都不能越过已关闭 bridge，也不再改变 caller/session
outcome。async custom callback 若吞掉 cancellation，Troupe 仍保证已提交的 `_ActCall`/Cue
handoff 不 join 它，但不承诺该任意用户 coroutine 自身终止；这与 trusted Python code 可以创建
不合作 task 的边界相同。

`session/cancel` 是 notification，不是完成确认。正常的 authoritative cancel 要求原
`session/prompt` 在所有 provider operations 已停止、尾部 update 已发送后返回
`stopReason=cancelled`。

### 7.4 Event-driven cancel settlement

Troupe 不为 cancellation 启动 timeout 或 escalation clock。得到 adapter-authoritative
terminal 时，session 按第 7.1 节回到 `Ready`；得到 EOF、process exit、transport failure 或
无法证明底层 turn 已停止的 terminal event 时，当前 turn 失败并进入 `Broken`，随后立即
执行 destructive process cleanup。若 agent 和 transport 都保持沉默，本轮就保持
`Cancelling`，supervisor operation 与 session-turn lease 一直保留；caller admission、Cue
child operation 和已取消的 Python task 已在本地 handoff 后释放或完成。

settlement 由 supervisor-owned operation 驱动，已经脱离原 Python task 的 cancellation
传播。后续 `act()` 可以取得 caller admission，但会在同一个 lossless availability barrier
无限等待，直到 supervisor 发布 `Ready/Broken/Closed`；它不能提交重叠 prompt。调用方自行
实现的 timer 只产生普通 cancellation intent；对已提交 turn，`act()` 在本地 handoff 后抛
`asyncio.CancelledError`，但这不承诺 Actor 在该时刻已经可复用。

某些 provider adapter 会用自己的内部 timer 合成 `cancelled`，但底层 query 可能尚未停止。
Troupe 不与该 timer 竞速；它只检查收到的 event。只有 typed evidence 能证明底层工作已经
停止时，该 response 才 authoritative。否则它是 `Uncertain` event，Actor 进入 `Broken`
并立即执行 destructive cleanup，不能因为字符串 `stopReason=cancelled` 复用 session。

Actor capability destruction 或 Production shutdown 是独立的 destructive authority，可以抢先结束
上述无限 settlement wait：Runtime revoke result route、关闭 ACP transport、强制终止并
reap 专属 guardian-owned process tree。这里的清理由显式 lifecycle event 触发，不由 elapsed
time 触发。

### 7.5 Cancel/result race

竞态线性化点是 Python result commit：

- cancellation acceptance 的线性化点就是同一 operation state 上的
  `CuePending -> SupervisorOwned` handoff commit。它与
  `CuePending -> CueOutcomeCommitted` 的 Python result commit 竞争；二者只能有一个先提交
  caller outcome。
- result 已 commit 后到达的取消不反向撤销成功。
- MCP handler 将 value 写入 `Accepted` 只是中间状态，不是 Python result commit；它仍需
  等 authoritative `end_turn`，也仍可能被已接受的取消抢先。
- custom `validate()` completion 也只是 acceptance 前的中间事件。每次 await 后必须重新读取旧
  slot/bridge generation；cancel handoff 先关闭 bridge 时，callback 的 success 或 rejection 都被
  丢弃，不能再写入 `Accepted` 或增加 invalid-call count。
- result commit 前接受的取消先完成本地 supervisor handoff，再向 caller 返回
  `asyncio.CancelledError`；即使远端稍后自然完成，也只用于恢复 session consistency，不再
  向 caller 返回成功。
- 能确认远端 terminal context 时 session 回到 `Ready`。
- connection 丢失且不能证明 exact context 可恢复时 session 进入 `Broken`。
- cancel 与正常 completion 竞争时，若 authoritative `end_turn` 先到但 result 尚未
  commit，caller 仍得到 cancellation；authoritative evidence 随 handoff 一起交给 supervisor，
  后者可以立即 settle 为 `Ready`，无需再向已经结束的 remote turn 发送 cancel。

## 8. 生命周期销毁、故障与恢复

### 8.1 Actor capability 销毁（内部生命周期）

`ActorCapability` 逻辑销毁时：

1. session 原子进入 `Closing`，拒绝新 turn。
2. 将 result route 标记为 closing 并 revoke capability，拒绝新的 result submission；取消
   Opening request，或抢先终止 Cue-owned / supervisor-owned 的当前 turn operation。
3. 不等待 `session/close` 或其他 graceful response；一 Actor 一 process 且第一版不做
   persistence，destruction event 已明确放弃该 session context。
4. 关闭 ACP connection。
5. 立即强制终止并回收整个 guardian-owned agent process tree，包括脱离 adapter process
   group/session 的后代；等待
   OS-confirmed exit/reap，不用 wall clock 猜测完成。
6. disarm slot、释放任何 caller admission/session-turn lease、关闭 MCP server-side session
   并 revoke bearer capability；所有 availability waiter 得到 terminal lifecycle failure。
7. session 进入 `Closed`。

Rust `Drop` 本身不 await。它只把 cleanup lease 移交 supervisor；lease 可以
暂时持有 native process resource，但不能保留可访问的 Actor capability。

每次实际 adapter 启动在一个专属 Linux guardian 后面。guardian 设置
`PR_SET_CHILD_SUBREAPER`，通过 private control pipe 接受 parent shutdown/EOF，并一直持有后代
ownership，直到 adapter root 与所有重挂接后代都被终止、`waitpid` 明确返回 `ECHILD` 后才退出。
这覆盖 adapter 在 Ready 前退出、退出前最后一刻派生进程，以及后代通过 `setsid()` 脱离原
PGID 的情况。Runtime 跟踪并等待 guardian handle；adapter root 一旦 reap，任何 cleanup 都不再
向其数字 PID/PGID 发信号，从而不可能误伤 PID reuse 后的无关进程。guardian 是 lifecycle
ownership primitive，不是 sandbox 或 authorization boundary。

fork child 不得在 Drop 中杀死 parent 创建的 ACP process，也不得对 parent 的 socket/pipe
执行 `shutdown`、接受连接或修改 parent state。所有 process/listener owner 必须记录 host PID，
并复用 Troupe 现有的 cross-process rejection 思路。

仅做 owner-PID rejection 还不够，因为 `fork()` 会复制 descriptor table。native Runtime
必须注册 `ForkFdRegistryV1` at-fork handlers：prepare handler阻止 Troupe-owned FD set变化；
parent handler只解除该保护；child handler不分配内存、不取得 Python lock，只对 registry
快照中的 child-local listener、accepted socket、ACP pipe/process handle、workspace lease和
其他 Troupe-owned descriptor副本调用 async-signal-safe `close(2)`，清空 child-local registry并
提交 owner-PID mismatch。child handler绝不调用`shutdown`、kill、wait/reap或触碰parent内存中的
lifecycle outcome。deterministic lifecycle test必须让fork child在parent shutdown期间继续存活，并证明parent仍
获得pipe EOF、完成process reap且listener port可重新bind；这样 child 的副本不会延长parent资源寿命。

Production shutdown 并发触发全部 Actor 的上述 destructive cleanup，并等待 OS-confirmed
exit/reap 后再停止共享 `ResultMcpService` 和 listener；它没有 global deadline，极端 OS
cleanup failure 可以使 shutdown 一直等待。listener 不因最后一个暂时可见 Actor 被 GC
而自行换端口重启。

### 8.2 故障恢复

第一版明确不支持跨 Troupe process restart 的 Actor/session persistence。Troupe process
结束意味着内存中的 Actor capability、Python Actor state、result route、turn ownership 和
agent session identity 一起失效；重启后创建的同名 Actor 是新 identity 与全新 session。

第一版中，任意 post-Ready EOF、connection loss、adapter/process crash 或 result
listener/route loss 都进入 Actor-lifetime `Broken`。即使 initialize snapshot advertise
load/resume，Troupe 也不在故障路径自动重启 adapter 或调用
`session/load`/`session/resume`；这些方法是否能恢复
同一个 conversation history，不等于它们能证明中断 turn 的工具、副作用和尾部事件已
精确 settlement。

以下行为被明确禁止：

- 自动调用 `session/new` 创建空白 session 后继续。
- 失败后静默切换 provider、model 或 ACP executable。
- 为了通过 schema 而在新 session 重放 script。
- 把 name 相同的新 Actor 当成旧 Actor context。
- 暴露 public `load()`、`resume()`、`recover()` 或隐式 recovery option。

未来若需要 persistence/recovery，必须整体设计 Production/Actor state checkpoint、provider
session identity、durable turn/settlement/result-commit journal、外部副作用幂等，以及恢复失败
时的显式 authority。只保存 provider session ID 或直接接上 ACP `session/load` 不构成
correct recovery。

## 9. 预登录、permission 与 trust boundary

### 9.1 Provider CLI 必须预先登录

ACP 只定义 client 与 agent 的通信，不替 Troupe 定义认证。Codex、Claude 和 Kimi 都要求
部署方在 Troupe 外使用对应 CLI 支持的方式预先完成登录。Troupe 只启动 pinned ACP agent，
使用 provider CLI 已有的登录态；调用方不通过 `AgentProfile`、`cast_actor()` 或 `act()` 传入
API key、token、credential reference 或认证选项。

Troupe 不发现环境中的 credential，不判断其来源，不保存认证 method，不调用 ACP
`authenticate`，也不提供登录 UI、轮询、刷新或重试流程。`session/new` 明确返回
`auth_required` 时，readiness 进入 terminal `AuthRequired`，当前及后续 `act()` 得到同一个
typed error。部署方必须在 Troupe 外修复 CLI 登录态并创建新的 Actor；原 Actor 不会 reopen。

### 9.2 Permission 与 elicitation

ACP agent 可能请求 tool permission 或向用户 elicitation。Actor turn 在后台运行，
不能假设存在 IDE 对话框。第一版把“完全非交互”定义为 Runtime invariant，不作为
`AgentProfile` option，也不提供 Python approval callback：

- Troupe 不 advertise ACP elicitation capability。
- 每个固定版本的 provider adapter 把 reverse request 分类为 tool authorization、
  plan review、provider question 或 unknown；不能只按 option 顺序或显示文本猜测。
- tool authorization 自动选择该 adapter 验证过的唯一 `allow_once`。不选择
  `allow_always`，因此不会把一次响应静默变成 provider session policy。
- plan review 自动选择 provider 的 canonical implement/continue option。
- provider question 自动选择 skip/reject，让 agent 根据已有上下文自行决定；prompt
  同时明确要求 agent 不询问用户并作出合理假设。
- unknown request 立即 reject/cancel。若 agent 无法继续，本轮以 typed
  `AgentTurnError` 结束，但不存在等待人工处理的中间状态。
- caller 或 Cue 在 reverse request 尚未响应时取消 turn，Troupe 返回 ACP
  `cancelled` outcome，并继续执行正常的 turn settlement。

ACP 的 `allow_once`、`reject_once` 等 option kind 只是客户端语义 hint，不能单独证明
请求意图。例如固定 adapter 可能用 permission request 承载 plan review 或问题选项。
  所以分类和响应规则属于每个 pinned adapter 的 conformance contract；缺少预期 option
  或出现歧义时必须在该 typed request 的同步 decision path 中立即失败，不能选择第一个
  选项，也不能等待人类。

当前固定 adapter 使用正常的 request-capable mode，而不是暴露 provider mode 给
`AgentProfile`：

| Agent | internal mode | Troupe 行为 |
|---|---|---|
| Codex | `agent` | workspace 内正常执行；出现 authorization 时自动响应 |
| Claude | `default` | 保留 Claude harness 判断路径；permission 自动响应 |
| Kimi | `default` | 保留 Kimi harness 判断路径；permission 自动响应 |

`initial_mode` 是 internal launch/adapter contract的一部分。`AgentLaunchSpec` 固定上述 exact
value和`ModeApplicationV1`。ACP config option是首选路径：launch spec固定exact config ID/value，
Opening总是发送一次`session/set_config_option`并校验完整返回snapshot。只有内置adapter没有usable
mode config option时，launch spec才可固定legacy mode
ID；Opening校验`availableModes`后发送一次`session/set_mode`并要求correlated success response。
Codex固定launch environment只是pre-session输入，仍须完成其中一个session-level typed apply。
两条session路径都必须在首个user prompt前完成并由deterministic/live tests覆盖；不能依赖provider
default恰好相同，也不能把mode或application mechanism暴露给`AgentProfile`。adapter无法显式
应用和确认目标mode时，Opening以`ConfigurationInvalid`失败。

未来若增加交互式产品形态，应设计独立 frontend contract；不能改变现有 Actor 的
无人值守语义。

### 9.3 Trusted execution model

V1 不在 Troupe 内实现 sandbox，也不承诺 filesystem、network、
subprocess 或 MCP isolation。ACP permission responder 的职责只是让后台
agent 不等待人类，它不是 authorization 或 security policy。

具体运行语义是：

- ACP adapter、provider harness 及其 tool subprocess 使用 Troupe process 当前的 OS
  identity；除非 provider 自身或外部部署环境另有限制，它们可以访问该身份能够访问的
  filesystem、network、process 和既有 CLI 登录态。
- `AgentProfile.workspace` 的同一 handle-stable directory identity 同时约束 child process
  cwd 和 `session/new.cwd`；canonical public path与internal procfd alias都不是 filesystem
  root、mount boundary 或访问控制规则。
- Troupe 自动选择 `allow_once` 可能允许 provider 继续执行 workspace 之外的操作；
  自动 permission response 不表示 Troupe 已限制副作用。
- `output_schema` 只约束返回值，不能约束工具副作用。
- custom `SchemaValue` 是调用方自己的受信任 Python code，可以访问数据库、网络、filesystem
  或其他进程内资源。Troupe 不保证 callback 纯净、幂等、终止或与其 rendered prompt 一致；
  这些都是扩展作者责任，且 callback 不能把 schema 变成 isolation boundary。
- Troupe 仍负责关闭 ACP connection、终止并回收 Actor-owned guardian process tree，以及执行
  output/resource bookkeeping；这些是 lifecycle guarantees，不是隔离。

因此第一版只适合运行调用方信任的 script、workspace 和 provider harness。需要安全
隔离的部署必须把整个 Troupe process 放进 container、VM、独立 OS account 或其他
外部边界。Troupe 不探测这些边界，也不因边界不存在而拒绝启动。

## 10. 时间与资源边界

Troupe V1 在所有层级都不通过 elapsed time 推断 operation 已失败、卡死或应升级清理。
Runtime 只使用由已观察 byte、node、depth 或 count 触发的确定性资源边界。已经发生
transient failure 后的 retry delay 只推迟下一次 attempt，不负责判定当前 attempt 失败。

### 10.1 No-timeout boundary

Troupe V1 不提供 timeout/watchdog：

- `Actor.act()` 没有 `timeout` 或 `deadline` 参数，`AgentProfile` 也不保存这类配置；
- readiness、Opening handshake、`session/new`、configuration、
  MCP readiness 和 provider turn 都可以无限等待；
- ACP frame read/write 与 loopback HTTP request/response 没有 read、write、idle 或 partial
  message timeout；
- custom `SchemaValue.validate()` 没有 Runtime-owned execution timeout；同步 callback 可以阻塞
  active Python loop，async callback 可以永久 pending，业务 deadline 由 callback/调用方自行组合；
- cancellation settlement、Actor capability cleanup、process exit/reap 和 Production
  shutdown 没有 grace period、cutoff 或 convergence deadline；
- 不定义 lifecycle clock policy 或 timeout exception；
- elapsed time 绝不能参与错误分类、retry eligibility、`Broken` 判定或 kill 决策。唯一的
  pacing transition 是第 4.4 节：typed failure
  已经决定retry并完成cleanup后，`OpeningRetryBackoffV1` delay结束只把`BackingOff`推进到下一
  `Opening` attempt；它不生成或升级failure。

Runtime 无法从沉默中区分“仍在合法工作”“provider 内部阻塞”和“协议对端失活”。这不仅
适用于长 turn，也适用于启动、协议 I/O 和清理。没有可靠 terminal evidence 时，时间经过
本身不是新的事实。

### 10.2 Event-driven lifecycle

所有 lifecycle transition 都由可命名事件驱动：

| 当前 operation | 允许驱动状态变化的事件 | 一直沉默时 |
|---|---|---|
| Opening phase | validated response、typed error、ACP EOF、agent process exit、Actor capability destruction 或 Production shutdown | 保持 `Opening`，所有 readiness waiter 继续等待 |
| idle Ready session | ACP EOF、agent process exit、listener/service loss、Actor capability destruction 或 Production shutdown | 保持 `Ready` |
| caller-owned submitted turn | correlated ACP terminal response、typed protocol/transport failure、agent process exit、result-tool event、caller/Cue cancellation 或 lifecycle destruction | 保持 `Active`，caller operation 继续持有 admission 与 session-turn lease |
| submitted-turn cancellation handoff | 本地 atomic owner transition | handoff commit 后 caller/Cue 完成；Actor 保持 `Cancelling`，supervisor 持有 session-turn lease |
| custom schema validation | sync/async `None` completion、`ValueRejected`、unexpected callback exception、caller/Cue cancellation 或 RunBinding teardown | bridge 与当前 result request 保持 pending；不生成 invalid result、failure 或 timeout |
| supervisor cancel settlement | adapter-authoritative terminal、明确的 protocol/transport/process failure，或 lifecycle destruction | 保持 `Cancelling`；later `act()` 只等待 availability，不启动 replacement session |
| partial ACP/HTTP message | 完整 bounded message、确定性 byte/node/count limit violation、peer EOF/reset 或 service close | 保持连接及其 HTTP permit 或 ACP stream ownership |
| Actor capability destruction / Production shutdown | destruction/shutdown intent 本身就是 destructive authority；立即 revoke result route、关闭 ACP transport、force terminate 并 reap dedicated guardian process tree | 等待 OS-confirmed exit/reap，不因经过时间而假装完成 |

pre-Ready 收到明确的 transient failure 后，Runtime 才能清理该 attempt 并进入
`BackingOff`；backoff 只是下一次尝试前的节流。未收到 response、error、EOF 或 process
exit 时，Opening attempt 不得因“等得太久”被转写为 transient failure。

post-Ready 的明确 protocol/transport failure 表示会话已不能可靠使用，可以立即把 Actor
标为 `Broken` 并执行 destructive cleanup；依据是已发生的错误事件，不是计时。Actor
capability destruction 和 Production shutdown 本身也是明确的销毁事件，因此无需先等待
`session/close` response。

provider adapter 自己产生的 synthetic cancel/error 只作为外部协议事件处理。Troupe 必须
根据 adapter contract 判断它是否代表底层工作已经停止；只有可机读证据成立才可作为 authoritative
settlement，证明不成立则按 `Uncertain`、`Broken` 和立即 destructive cleanup 处理。
Troupe 不设置 timer 与 adapter 竞速。

### 10.3 Caller-owned cancellation 与 hard stop

业务 deadline 由调用方在 Troupe 之上的 flow 中组合 task、Cue 或 Python async
primitive；这个 timer 不属于 Runtime。它向 Runtime 表达普通 cancellation intent，而不是
“经过 N 秒后可以假装底层工作已经停止”。

- prompt 尚未提交时，取消本地 readiness waiter 即可返回；
- prompt 已提交后，Runtime 先把 exact turn 本地原子移交给 Actor supervisor；handoff 不做
  网络 I/O，commit 后 `_ActCall` 立即抛 `asyncio.CancelledError`，Cue drain 不再等待远端；
- supervisor 在尚未观察到 authoritative terminal 时确保发送且只发送一次
  `session/cancel`，保留 session-turn lease 并等待 authoritative settlement；期间 Actor
  保持 `Cancelling`；
- 如果 provider 永不回应，supervisor operation 和 Actor 的 `Cancelling` 状态可以永远不
  完成，但已取消的 caller/Cue 不受影响；后续 `act()` 等待 availability；
- V1 不提供 public per-Actor `close()`、`retire()` 或 `destroy()`；Production shutdown
  对全部 Actor 执行 destructive cleanup，并以 OS-confirmed process exit/reap 作为完成条件。

调用方 timer 可以通过取消让当前 `act()` 在本地 handoff 后结束，但不能据此推断 agent
已停止或 Actor 已可接收下一轮。Runtime 不为 backend settlement 或 Production cleanup
增加隐式 deadline。

### 10.4 `ResourceLimitsV1`

`ResourceLimitsV1` 是当前 Runtime build 固定的 contract。V1 不把这些值放进
`AgentProfile`、Production user config、provider option 或 `act()`；所有 Actor 和 adapter
使用同一份 immutable profile，也不提供 override。

`KiB` 和 `MiB` 使用 1024 进制。固定边界为：

| Resource | `ResourceLimitsV1` |
|---|---:|
| `script` | 256 KiB UTF-8 bytes |
| compiled act schema | depth 32；1,024 IR nodes；全部 nested object 合计 512 fields |
| 单个 schema description | 1..1,024 Unicode scalar values；最多 4 KiB UTF-8；必须包含非空白字符 |
| scalar choices | 每 descriptor 最多 256 个；每 compiled schema 合计最多 4,096 个；canonical encoded choice data 合计最多 256 KiB |
| 单个 custom prompt fragment | 1..16 KiB UTF-8；必须包含非空白字符 |
| rendered prompt envelope | 1 MiB UTF-8 bytes |
| encoded ACP JSON frame | 16 MiB |
| ACP/MCP protocol JSON nesting | depth 64 |
| MCP HTTP request line + headers | 32 KiB raw bytes |
| MCP HTTP body | transfer decoding 后 8 MiB |
| result `value` | depth 32；65,536 JSON nodes |
| 单个 result string | 1,048,576 Unicode scalar values 且 4 MiB UTF-8 bytes |
| 单个 result list | 10,000 items |
| invalid `troupe_submit_result` calls | 每 turn 8 次；第 9 次触发 structured cancellation |
| 单个 `ValueRejected` message | 保留最多 4 KiB UTF-8，超出部分截断并设置 validation detail truncated |
| 单次 validation detail | declaration/path order 的前 16 条；Runtime 生成的 response envelope/content 最多 32 KiB，不计必须原样回显的 client `RequestId` bytes |
| ambiguous Opening crash loop | 相同 normalized fingerprint 连续 3 次；前 2 次重试，第 3 次 `StartFailed(code="crash_loop")` |
| live result HTTP connections | Production-global 65,536；无 per-Actor limit |

schema depth 以 root object 为 1；每个 nested object、list 或 wrapper IR node 增加一层。
schema node count 包含 root、每个 field value descriptor、container 和 wrapper；field total
单独统计所有 object keys。result depth 同样以 `value` root 为 1；每个 JSON scalar、array
或 object 是一个 node，object key 不另算 node。protocol depth 64 作用于包含 JSON-RPC/MCP
envelope 的完整 frame/request，result `value` 仍单独受 depth 32 约束。

description 与 custom fragment 都按原始 UTF-8 编码计数，不 trim/normalize 后重新计数；
non-whitespace 只决定空值合法性。choice count 在 wrapper 展开后的每个 scalar descriptor 上统计；
choice bytes 使用 prompt renderer 的 deterministic canonical scalar encoding，既进入 256 KiB
choice aggregate，也进入最终 1 MiB prompt envelope。custom validator invocation 次数不另设
capacity：每个匹配 result node 最多调用一次，并由 result 的 65,536-node bound 封顶。

JSON-RPC request 的 `id` 只要是 JSON string 或 number 就是有效 correlation value，Runtime 必须在
对应 response 中原样回显。client-controlled `RequestId` 的 encoded bytes 不占上表的 validation-detail
budget；超长 `RequestId` 不能导致 request rejection、`id: null`、slot 语义变化或 detail 进一步截断。

`StrValue(...)` 或 `ListValue(...)` 未写业务 maximum 时，compiler 使用上述 string/list maximum
作为 effective bound，并把它同时交给 prompt renderer 和 Rust validator。调用方显式给出
超过 Runtime maximum 的 `max_length`、`min_length`、`max_items` 或 `min_items` 时，同步
preflight 抛 `ValueError`，不能静默 clamp。string 的 schema length 按 Unicode scalar
value 计数，4 MiB UTF-8 bytes 是额外的 encoded resource bound。

### 10.5 超限语义

- `script`、schema 或 rendered prompt 在 prompt 提交前超限：同步 `ValueError`，不 arm
  result slot，不改变 session context。
- descriptor description/choices 在 constructor 或 graph aggregate 校验超限时同步
  `ValueError`。custom `render_prompt()` 的 exception、错误 return、空白或 fragment 超限改为
  `SchemaCallbackError(phase="render_prompt")`；同样发生在 prompt 提交前。
- inbound ACP frame 或 protocol JSON depth 超限：这是 observed protocol failure。Ready 前
  进入 `AgentSessionStartError(code="resource_limit")`；Ready 后当前 turn 失败且 Actor
  进入 `Broken`。
  outbound prompt 已由 preflight 保证不会超限。
- HTTP request line/header 超限返回 `431` 并关闭 connection；body 超限返回 `413` 并关闭
  connection。若还没有足够信息归属 exact tool call，`ResultSlot` 保持不变。
- 已归属当前 turn 的 result `value` 在 type、bound、depth、node、string 或 list limit 上
  失败时，slot 保持 `Awaiting`，返回 MCP tool execution error，并计入 invalid-call count。
  error detail 只包含固定顺序的前 16 条；Runtime 生成的 response envelope/content 达到 32 KiB 时
  截断并标记 `truncated=true`，mandatory exact-echo `RequestId` bytes 不参与该预算。
- `ValueRejected` message 在 UTF-8 boundary 上截断到 4 KiB 后作为 `custom_validation` issue；
  rejection 仍完整成立。unexpected callback exception 不进入 validation detail，不计 invalid-call，
  并走第 6.4.3 节的 local callback-fault handoff。
- 前 8 次 invalid call 允许同一 agent turn 修正。第 9 次把 result contract 提交为 terminal
  `AgentResultError`。Runtime 使用同一个 supervisor handoff 收尾 remote turn：caller 得到
  typed error，supervisor 发送一次 `session/cancel`，并按无 timeout 规则等待 authoritative
  settlement、明确 failure event 或 lifecycle destruction；在此之前不释放 session-turn
  lease。
- HTTP connection limit 统计已经 accept 且尚未关闭的 live sockets，不预分配 65,536 个
  connection object。到达全局上限时立即拒绝/关闭新 connection，不解析 request；不做
  Actor 归属、per-Actor quota 或 fairness。OS file-descriptor/resource limit 可以更早拒绝
  connection，Troupe 不承诺部署环境一定能实际达到 65,536。

partial ACP frame、partial HTTP request 或低于上述 byte 上限的 connection 仍可无限保持；
pending custom callback 同样可以无限保持；resource limit 不得被改写成 inactivity timer。

### 10.6 Capacity 与 retention boundary

V1 除 HTTP listener 的 global live-connection count 外，不实现 Runtime-owned capacity
scheduler：不限制或排队 agent process、Opening attempt 或跨 Actor active turns，也没有
global/per-provider turn semaphore。部署方通过 Actor 数量、container、VM、OS account、
cgroup 或其他外部机制约束总 CPU、memory 和 process count。

Runtime 不保留 per-turn audit/update collection，因此没有 audit event count/byte limit。
correctness control state 只保存 result slot、correlation/settlement evidence、unresolved
request state、custom bridge/task lease、invalid-call count 与有界的 validation issue summary。
assistant text、ordinary updates、usage 和 tool trace 在完成分类后丢弃。完整 diagnostics、trace
和持久化 audit 不属于 V1；如有需要，后续独立设计。

## 11. Provider profiles

以下是 2026-08-03 的实现调研快照，不是长期版本承诺。当前 build 的具体 selector 由私有
`AgentLaunchSpec` 固定。

| Provider | ACP executable | 核心 runtime 路径 | 当前版本基线 | HTTP MCP 注入路径 |
|---|---|---|---|---|
| Codex | `@agentclientprotocol/codex-acp` | Codex App Server -> Codex core | adapter 1.1.9，声明 `@openai/codex ^0.145.0` | `session/new.mcpServers` -> thread-start `mcp_servers`，支持 HTTP |
| Claude | `@agentclientprotocol/claude-agent-acp` | Claude Agent SDK -> Claude Code core | adapter 0.64.2，SDK 0.3.220，Node >=22 | `session/new.mcpServers` -> SDK `Options.mcpServers`，支持 HTTP |
| Kimi | `kimi acp` | Kimi Code harness | Kimi Code 0.31.1，ACP SDK 0.23.0 | `session/new.mcpServers` -> v1 harness `createSession`，支持 HTTP |

### 11.1 Codex

- `codex-acp` 是独立 ACP executable，不是 Codex CLI 自带的
  `codex acp` subcommand。
- Troupe 内部通过固定 package version 的 `npx --yes` 启动它；package
  名称、版本和 npx 参数不进入 public `AgentProfile`。
- 默认不读取任意 global Codex，也不允许未验证的 `CODEX_PATH` 覆盖。
- Codex 的 commentary/final phase 在完成协议分类后丢弃，不是 result correctness boundary。
- real acceptance suite 必须验证 session-scoped HTTP MCP initialize、tool discovery、
  成功提交、validation error 后重试、project instructions、tools、approval、两轮 context、
  cancel 后下一轮和 process cleanup。另以未预登录的 isolated real child 证明缺少认证时只得到
  typed `auth_required`，且没有browser、terminal prompt、device-login、permission或elicitation活动。

### 11.2 Claude

- adapter 是独立 Node executable，要求 Node >=22。
- Troupe 内部同样通过 build-internal fixed package version 的 `npx --yes` 启动 adapter。
- Troupe 只执行内置 `AgentLaunchSpec` 的标准 launcher；不加载交互 shell profile，也不执行
  用户启动脚本。launcher 继承父进程环境。
- `setproxy` 不是 Claude adapter 的启动前置条件；Troupe 不检测、不调用也不验收
  这个命令。若部署环境自行配置了通用代理环境变量，它们只通过上述父进程
  环境继承自然生效。
- 它通过 Claude Agent SDK 使用 Claude Code preset、user/project/local settings、
  tools、hooks、MCP 与 subagents，因此走的是核心 harness 路径。
- adapter 必须使用部署方预先建立的 Claude CLI 登录态，不能由 Troupe 启动登录 UI 或认证流程。
- 当前固定 adapter 的 SDK query 在 `session/new` 完成后才可能于首个 prompt 建立 provider
  连接。因此隔离的未登录 child 若在 Opening 得到 ACP `AuthRequired`，返回
  `AgentAuthenticationRequiredError(authentication_required)`；若在首个 prompt 才得到
  `AuthRequired`/`authentication_failed`，返回
  `AgentSessionBrokenError(authentication_lost)`。两种都是 terminal，Troupe 都不发送
  `authenticate`，也不启动 browser、terminal 或 device-login UI。
- 当前固定 adapter 0.64.2 对不支持 effort level 的 advertised model 可以省略整个 `effort`
  config option。Claude launch spec 因此固定 `effort_option_optional_when_unspecified=true`：仅当
  `AgentProfile.effort is None` 且 model apply 后的完整 snapshot 确实不含该 option 时，effective
  effort 冻结为 `None`。显式请求 effort 时 option 仍必须存在；present option 的 type、domain 与
  non-null current value 仍按 exact contract 校验。
- `EnterPlanMode` 在 submitted turn 内依次产生 `current_mode_update=plan` 和 mode current value 为
  `plan` 的完整 `config_option_update`，随后 `ExitPlanMode` permission 才提供回到 `default` 的 option。
  Troupe 选择 `default` 后，provider 必须再依次发送 `current_mode_update=default` 和 mode current value
  为 `default` 的完整 `config_option_update`。Claude adapter 只在该 prompt response 尚未到达时把这组
  `default -> plan -> default` 过渡视为合法；两份完整 snapshot 中的 model/effort 仍按冻结值严格校验。
  turn settlement 只有在两种 mode 表示都恢复到 `default` 后才可发布 `Ready`；缺失、乱序、idle
  `plan`、其他 mode 或其他 provider 的 mode drift 都是 `protocol_violation`。prompt response 的到达会
  封闭尚未完成的过渡；其后的迟到 `default` 通知不能再修复该 turn。
- adapter 支持的 load/resume/fork/list/close 等 optional 能力只按实际 initialize
  snapshot 使用，不上升为 Actor 第一版最低契约。
- real acceptance suite 必须证明 caller-provided HTTP MCP config 在首个 user prompt 前进入 Claude
  SDK query 并完成 tool discovery，且 tool execution error 能回到同一 agent turn。
- live test 在互相隔离的temporary HOME/workspace中分别安装distinct、完全受控的 user
  `~/.claude/settings.json`、project `.claude/settings.json`和local
  `.claude/settings.local.json` hook fixture；一个真实prompt必须让三个source各写一次不同的
  deterministic marker。真实 credential store 以及 user settings 中 allowlist 内的既有认证/endpoint
  env 可以只读保留以复用预登录状态，但 ambient user settings、其他 env、hook 或 plugin 不得合入
  fixture；任一source被忽略、合并或由Troupe伪写marker都失败。
- 当前固定的 adapter 0.64.2 有自己的 30 秒 force-cancel backstop，可能在底层 SDK query
  仍卡住时合成 `cancelled`。Troupe 不设置一个更早的 timer 与它竞速；收到该 response 后，
  若没有可机读的 healthy-cancel evidence，就归一化为 `Uncertain`、进入 `Broken` 并立即
  cleanup。这个 synthetic response 不得使 session 回到 `Ready`。

### 11.3 Kimi

- ACP server 内建于 Kimi Code CLI，不需要额外 `kimi-acp` adapter package。
- 由部署方在启动 Troupe 前使用 `kimi login` 完成登录。
- 当前实现覆盖 initialize、new、prompt、cancel、load/resume/list、model、thinking、
  MCP、filesystem 和 permission 等主要路径；shell command 由 Kimi 本地执行，未接
  ACP terminal reverse-RPC。
- 当前没有 `session/close`，fork 未实现；在一 Actor 一 process 拓扑下不是
  第一版 blocker。
- model 与 thinking 已通过独立的 session config options 暴露；旧版
  `model,thinking` 组合 ID 只属于 compatibility path，Troupe 不使用。
- Kimi 0.31.1 对不支持 thinking 的 advertised model 可以省略整个 `thinking` option。
  Kimi launch spec 因此固定 `effort_option_optional_when_unspecified=true`：仅当
  `AgentProfile.effort is None` 且 model apply 后的完整 snapshot 确实不含该 option 时，
  effective effort 冻结为 `None`；显式 effort 和 present option 仍执行 exact domain/current
  value 校验。
- `Command.exact_version="0.31.1"` 是运行时 opening contract，不只是 release metadata。
  initialize response 必须包含 exact `agentInfo.version="0.31.1"`；缺失或不匹配都在
  `initialize` 阶段以 `protocol_incompatible` terminal failure 拒绝，不能继续发送
  `session/new`。
- 普通 authorization 只有 exact `approve_once`/`approve_always`/`reject` shape 才选择
  `approve_once`；question 只有连续的 `q0_opt_N` 加 `q0_skip` shape 才选择 `q0_skip`；plan
  review 只接受固定 approve/revise/reject shape，或连续 `plan_opt_N` 加 revise/reject shape 并
  选择 `plan_opt_0`。缺项、重复、未知 ID/kind 都不按顺序猜测。
- Kimi 没有 ACP terminal reverse-RPC。任何未注册的 reverse method 都得到标准 JSON-RPC
  `-32601 Method not found`。在同一 submitted turn 的 prompt response 尚未到达时，这类明确
  拒绝本身不污染仍可结算的 turn；prompt response 已到达或不存在 active turn 时，未知 request
  越过了 turn ownership boundary。response 后同一 JSON-RPC batch 中的 request 在 raw frame
  dispatch 前预登记 deferred evidence；当前 turn 保持 pending，既不能先返回 accepted result，也
  不能在 error response 完成 transport flush 前提交 `Broken`。flush 后当前 turn 的同一次原子
  settlement 必须消费该 boundary evidence：即使 `session/prompt` 本身是 well-formed `Ok` response，
  caller 与 session health 也一起提交为 `protocol_violation`，不能依赖另一个异步 terminal task 稍后
  抢锁修正。ACP 没有 peer-consumption ACK，flush 不承诺 agent process 已读取 response。
  没有这种 boundary violation 时，Kimi 0.31.1 的 well-formed prompt response（包括 `cancelled`）是
  authoritative settlement evidence，因而 caller cancellation 结算后可以继续使用同一 session。

Kimi real acceptance suite 要求对内置 Kimi Code selector 完成 session-scoped HTTP MCP
initialize/discovery/call/error correction、typed notification、permission、两轮 context、
cancel reuse 和 process cleanup。assistant 文本完成分类后丢弃，不参与 result selection。另以
未预登录的 isolated real child 证明缺少
认证时只得到typed `auth_required`，且没有browser、terminal prompt、device-login、permission
或elicitation活动。

## 12. Dependency 与配置

### 12.1 Rust dependency

Troupe 可以直接依赖官方 `agent-client-protocol` Rust crate。当前审计版本为
2.0.0；crate package major 与 ACP wire v1 是两个独立版本概念。

该 crate 提供 protocol types、connection/request correlation 和 stdio/process 支持，
但不会安装 Codex、Claude 或 Kimi agent executable，也不会实现 Troupe 的 state
machine、HTTP MCP server、`ResultSlot` 或 typed act-schema compiler。

public result contract 不需要 Rust `jsonschema` crate。Troupe 对 closed built-in descriptor
set 编译 `ValueContract` 并做穷尽 native validation；public custom `SchemaValue` 则经过显式
Python bridge 执行用户规则。这是带 programmable escape hatch 的 product type system，不是
重新实现通用 JSON Schema。MCP SDK/HTTP stack 仍需解析 JSON-RPC 和静态 tool
`inputSchema`，两者不改变 public schema API。

Troupe 已有 Tokio runtime。新增 Rust dependency 至少还会包括支持 MCP HTTP server 的
协议/HTTP 实现。不能假设 ACP crate 同时提供
MCP server；Runtime 必须独立实现 adapter registry 中封闭的 stateful MCP wire revision set 与
`McpTransportProfileV1`，每条 route 必须固定其中一个 exact revision，Cargo lock中不得并存第二套
MCP server/JSON-RPC protocol types。built-in schema compiler 不依赖 Python
`json`/`jsonschema`；只有 graph 中显式存在 custom `SchemaValue` 时才创建 Python validation
bridge，不能让 built-in fast path 意外进入 GIL。

### 12.2 Internal agent launch registry

public API 通过 `agent` 选择 agent kind，但不选择其 launch mechanism。package runner、
package name、adapter version、启动参数、mode application 和 settlement profile 都由当前
build 的 private registry 固定：

```rust
struct AgentLaunchSpec {
    agent: AgentKind,
    acp_wire_protocol: AcpWireProtocolVersion,
    client_sdk_version: &'static str,
    mcp_wire_protocol: McpWireProtocolVersion,
    mcp_transport_profile: McpTransportProfileId,
    runner: LaunchRunner,
    initial_mode: &'static str,
    mode_application: ModeApplicationV1,
    settlement_profile: SettlementProfileId,
}

enum ModeApplicationV1 {
    SessionConfigOption {
        config_id: &'static str,
        value: &'static str,
    },
    LegacySessionMode {
        mode_id: &'static str,
    },
}

enum LaunchRunner {
    Npx {
        package: &'static str,
        exact_version: &'static str,
        fixed_args: &'static [&'static str],
    },
    Command {
        program: &'static str,
        fixed_args: &'static [&'static str],
        exact_version: &'static str,
    },
}
```

Codex 和 Claude 的标准路径仍是由 Troupe 直接执行固定顶层版本的
`npx --yes package@version`，用户不单独安装 adapter。Troupe 不执行 shell、不使用 `@latest`，
也不允许 public profile 覆盖 package、版本或启动参数。Kimi 的固定 CLI 命令同样封装在
registry 中；`Command` runner 的 `exact_version` 还必须由 initialize `agentInfo.version` 精确
证明，不能只相信 `PATH` 上同名 executable。传递依赖的处理不进入 Runtime API，也不是本设计的
运行时契约。

首次冷启动时，package acquisition 属于 Actor session 的 `Opening` 阶段；
`act()` 继续等待 shared readiness。supervisor 按 launch spec 的值相等性共享
首次 preparation gate，避免多个 Actor 同时冷启动同一 package。下载、cache 或 registry
错误按第 4.4 节分类：明确的 transient failure 在 shared gate 内退避重试；package/version
不存在等 deterministic failure 作为 immutable snapshot 留在 shared gate，并向当前与后续
waiter 提交 `StartFailed`。分类只消费 npm 的 exact error-code record，并使用 bounded streaming
parser 匹配 closed code set；不保留任意 stderr，缺失、未知或互相冲突的 code 仍按 ambiguous
process/EOF failure 处理，不能仅凭自然语言文案认定为 transient。

用户不需要单独安装 ACP adapter。Codex/Claude 仍要求系统存在 Troupe 支持的
Node/npm runtime；cast 在提交后台启动前解析并校验 `npx`。production 可以
预热或离线准备相同的 npm cache，但实际启动命令不变。

修改 private registry 只影响之后构建的包，不改变已经启动的 Actor。正式 API 不提供 version
override。

Runtime 不查询 ACP public registry 来决定启动版本，不解析 `latest`，也不接受 floating
semver。ACP/MCP crate package version、wire protocol version、provider executable version和
internal mode是不同概念；private registry与编译依赖分别表达它们。

### 12.3 `AgentProfile` 与 internal snapshot

public API 直接接收 immutable `AgentProfile` 对象。cast 不保存调用方随后
可变的 Python graph，而是把它解析成 provider-independent 的 internal snapshot。
V1 public shape 是：

```python
@dataclass(frozen=True, slots=True, kw_only=True)
class AgentProfile:
    agent: Literal["codex", "claude", "kimi"]
    workspace: str | os.PathLike[str]
    model: str
    effort: str | None
```

上述 Literal 是 V1 目标支持集合。runtime accepted values、type stub Literal 和 private
adapter registry 必须完全一致；不能暴露一个没有内置 adapter、只会以“experimental”或
best-effort 方式启动的 agent kind。

四个 constructor 参数都没有 Python default，调用方必须显式传入。`model`
必须是非空字符串；`effort` 必须是非空字符串或 `None`。例如：

```python
AgentProfile(
    agent="codex",
    workspace="/repo",
    model="gpt-5.6-sol",
    effort="max",
)
```

`effort=None` 表示在选定 model 后不发送 thought-level override，接受 agent
报告的当前值；present ACP option 的值必须是其 discriminated type 允许的非 null value。
它不表示关闭 reasoning。关闭支持该语义的 agent 必须使用其实际 advertise 的值，例如
Kimi 的 `"off"`。只有 pinned adapter contract 明确允许整个 effort option 缺失时，
internal effective value 才能是 `None`；present `currentValue: null` 永远不能表达 default。

`workspace` 定义该 Actor agent session 的工作目录，在 Actor lifetime 内不可改变。
cast snapshot 后，public identity 是 canonical path；native Runtime 同时持有该 directory
的 open handle。child process 通过 `fchdir(handle)` 使用该 inode，ACP
`session/new.cwd` 使用由同一 handle 支撑的 `/proc/<owner-pid>/fd/<fd>` absolute alias。
这里没有 package、adapter version、runner 或 executable 字段。

cast 使用 `os.fspath()` 解析 workspace。PathLike 必须返回 `str`，返回 `bytes` 或其他类型
时同步抛 `TypeError`；路径必须非空、可编码为协议字符串、是 absolute path，并且在 cast
时指向现有 directory。relative path、NUL 和非目录输入在 Actor publication 前同步失败，
不能相对于 ambient process cwd 解析。Runtime 在 cast transaction 中 canonicalize 路径、
解析 symlink，使用 `O_RDONLY | O_DIRECTORY | O_CLOEXEC` 打开并冻结 handle、owner PID、
`st_dev` 和 `st_ino`。
Opening 在 spawn 前、每次 `session/new` 前后把 pathname `stat` 与 `fstat(handle)` 比较并
检查 search/read access；删除、替换、非目录或失权是后台 Opening failure。实际 child/ACP
consumer只使用 handle-stable binding，所以检查与使用之间发生的 rename-and-replace 也不会
访问同名新目录；下一 checkpoint 会观察 mismatch 并 cleanup。Ready 后 handle 保持到 Actor
session 销毁，外部 pathname 变化不会把既有 session 切换到另一 directory，也不靠轮询推断故障。

`model` 和 `effort` 都是 provider-scoped ACP selector value，不是 Troupe
定义的跨 provider taxonomy：

- Codex model 通常是 model slug，effort 是当前 model advertise 的具体档位。
- Claude model 可能是 `default`、alias 或完整 ID；effort 可能包括 adapter 的
  `default` sentinel 和 model-specific levels。
- Kimi model 是 Kimi 配置中的 model alias；thinking 可能是 `off/on`，也可能是
  `off` 加 model 声明的具体 effort levels。

Troupe 不定义全局 effort enum，不做 `high -> on`、`max -> high` 等转换，也不接受
Codex `model[effort]` 或旧 Kimi `model,thinking` 组合格式。public 字段保持统一，
wire 差异由固定版本的 agent adapter 处理：

| Agent | model config id | effort config id | semantic category |
|---|---|---|---|
| Codex | `model` | `reasoning_effort` | `model` / `thought_level` |
| Claude | `model` | `effort` | `model` / `thought_level` |
| Kimi | `model` | `thinking` | `model` / `thought_level` |

ACP category 只是 UX metadata，不能作为 correctness key。每个 pinned adapter 使用
已知 config ID，并验证返回 option 的 type、候选值和最终 current value；category
在完成协议 decode 后直接丢弃，缺失或未知 category 不影响正确性。设置顺序固定为
model first、refresh full config snapshot、effort second，因为 effort 候选值依赖所选
model。

同步 cast 只验证 Python 类型、agent kind、path 和非空字符串等本地条件。model/effort
是否被固定 adapter 和当前认证账号支持，只能在后台 `Opening` 的 `session/new` 后验证；
显式请求的值不受支持时提交为 typed `StartFailed`，不得静默 fallback、coerce 或改用
provider default。`effort=None` 是调用方显式选择不 override，不属于 fallback。

internal snapshot 的概念类型为：

```rust
struct ResolvedAgentProfile {
    agent: AgentKind,
    command: ResolvedAgentCommand,
    workspace: WorkspaceLeaseV1,
    requested_model: String,
    requested_effort: Option<String>,
    environment: EnvironmentPolicy,
}

struct WorkspaceLeaseV1 {
    canonical_path: PathBuf,
    owner_pid: u32,
    st_dev: u64,
    st_ino: u64,
    directory: OwnedFd,
    acp_cwd_alias: PathBuf,
}

struct AppliedAgentSelection {
    requested_model: String,
    requested_effort: Option<String>,
    effective_model: String,
    effective_effort: Option<String>,
}
```

每次 cast 都产生自己的 `ResolvedAgentProfile`，即使多个 Actor 收到同一个
public `AgentProfile` 对象。`AppliedAgentSelection` 在 ACP configuration 成功后形成，
用于 readiness 和后续 config-drift 校验，但不反向修改 public 对象。会改变 session identity
的字段不能在 `act()` 时覆盖。第一版 `act()` 不接收 deadline/timeout override；调用方
需要的业务 deadline 由 Troupe 之上的 flow 通过 task/Cue cancellation 组合。

ACP permission responder 是 Runtime 内建行为，不是 `ResolvedAgentProfile` 中的
用户可选 policy。`environment` 只描述 Troupe 明确设置的 process launch 输入；继承的 provider
登录状态由 CLI 自己解释，不进入 profile，也不构成 isolation guarantee。

第一版不提供 `provider_options: dict`、`extra: dict` 或类似 escape hatch。未来确实需要
暴露 fast mode、Claude agent persona 或 Kimi thinking keep 时，应增加独立评审过的
typed option；ACP config ID、package 参数和任意 provider payload 不直接泄漏到 public
profile。

## 13. 内部接口

Troupe 内部不需要通用的多 backend abstraction；第一版可以把 ACP 直接作为唯一
transport，同时仍保持 protocol 与 Actor 层分离。ACP backend 内部按 harness 提供
`CodexAcpAdapter`、`ClaudeAcpAdapter` 和 `KimiAcpAdapter`，负责 launch contract、
session config 映射、capability validation、HTTP MCP mapping 和 settlement profile。
它们不形成每个具体 model 一个 adapter；model catalog 和 effort compatibility 必须
从当前 session 的 `configOptions` 数据驱动发现。

概念接口为：

```rust
trait AcpAgentAdapter {
    fn launch_spec(&self) -> &'static AgentLaunchSpec;
    fn model_config_id(&self) -> &'static str;
    fn effort_config_id(&self) -> &'static str;
    fn effort_option_optional_when_unspecified(&self) -> bool;
    fn mode_application(&self) -> &'static ModeApplicationV1;
    fn classify_reverse_request(&self, request: &AcpRequest) -> AutonomousRequestKind;
    fn resolve_reverse_request(&self, request: &AcpRequest) -> AcpResponse;
    fn settlement_profile(&self) -> SettlementProfileId;
}
```

Actor/session 层只处理统一的 requested/effective selection，不读取 provider-specific
config ID：

```rust
struct AgentSessionSlot {
    profile: Arc<ResolvedAgentProfile>,
    state: SessionState,
    readiness: SharedReadiness,
    availability: SharedSessionAvailability,
    caller_admission: AtomicActAdmission,
    session_identity: TroupeSessionId,
    result_route: ResultRouteLease,
    opening_join: Option<Arc<OpeningJoinV1>>,
    next_turn_index: AtomicU64,
    worker: AgentSessionWorkerHandle,
}

struct OpeningJoinV1 {
    provisional_generation: SessionGeneration,
    route_epoch: Arc<RouteEpochStateCell>,
    session_new: SingleAssignment<SuccessfulSessionNew>,
    configuration_ready: SingleAssignment<EffectiveSelection>,
    mcp_ready: SingleAssignment<McpDiscoveryEvidence>,
    ready_commit: SingleAssignment<ReadySessionSnapshot>,
}

struct AgentTurnRequest {
    operation_id: OperationId,
    turn_index: u64,
    script: String,
    schema: Arc<CompiledActSchema>,
    custom_validation: Option<PythonSchemaValidationBridgeLease>,
}

struct AgentTurnOutcome {
    settlement: TurnSettlement,
    accepted_result: Option<ValidatedActValue>,
    session_health: SessionHealth,
}

struct AgentTurnOperation {
    request: AgentTurnRequest,
    session_turn: SessionTurnLease,
    armed_result: ArmedResultLease,
    control: LinearizedTurnControl,
    supervisor_continuation: PreparedSupervisorContinuation,
}

enum LinearizedTurnControl {
    CuePending {
        caller_admission: ActAdmissionLease,
        child: CuedChildLease,
    },
    CueOutcomeCommitted {
        caller_admission: ActAdmissionLease,
        child: CuedChildLease,
    },
    SupervisorOwned {
        caller_outcome: DetachedCallerOutcome,
    },
}

enum DetachedCallerOutcome {
    Cancelled(CancelOrigin),
    Failed(CallerError),
}
```

`PreparedSupervisorContinuation` 在 prompt 发送前随 turn 一起建立，保证取消 handoff 只是
worker 内的线性化 control transition，不依赖临时 allocation 或可能背压的 command queue。
Python success/failure commit 走 `CuePending -> CueOutcomeCommitted`；取消或已经确定的 typed
abort 走 `CuePending -> SupervisorOwned`。这两个 transition 在同一个 state cell 上竞争，
不能同时成功。handoff 后原 `AgentTurnOperation` 的 remote 部分继续存在；旧 variant 中的
caller admission 与 `CuedChildLease` 只在 session `Cancelling` 已发布后释放。

`OpeningJoinV1`在发送每次`session/new`前创建，ACP worker只提交`session_new`和
`configuration_ready`，`ResultMcpService`只提交`mcp_ready`；任一producer提交后都可以尝试
`ready_commit`，但只有三个component属于同一provisional generation、route epoch仍live且post-new
workspace revalidation已经进入`SuccessfulSessionNew`时才能成功。route revoke与component commit在同一
epoch state上线性化：旧MCP completion若先发生也会被随后revoke阻止Ready，若revoke先发生则completion
被拒绝；两种顺序都不能影响下一次invocation的新`OpeningJoinV1`。

Production-owned result service 的核心接口概念上是：

```rust
trait ResultMcpService {
    fn endpoint(&self) -> &McpHttpEndpoint;
    fn register_invocation(&self, actor: ActorId, provisional_generation: SessionGeneration)
        -> ResultRouteLease;
    fn arm(&self, route: &ResultRouteLease, contract: ActiveResultContract)
        -> ArmedResultLease;
    fn acquire_request(&self, route: &ResultRouteLease) -> ResultRequestLease;
}

struct ResultRequestLease {
    session_generation: SessionGeneration,
    route_epoch: Arc<RouteEpochStateCell>,
    active_result: Option<(ArmGeneration, Arc<ResultSlot>)>,
}

struct PythonSchemaValidationBridgeLease {
    run_binding: RunBindingId,
    bridge_generation: BridgeGeneration,
    state: Arc<ValidationBridgeStateCell>,
    serial_dispatch: Arc<AsyncMutex<()>>,
}
```

`ResultRouteLease` 管理 bearer capability 的撤销，`ArmedResultLease` 管理 operation 终态时
的 tombstone/disarm。`ResultRequestLease` 在 body decode 前固定 route/session/arm snapshot，
并只允许 handler 使用捕获的旧 slot；其acceptance operation与revoke/disarm在
`RouteEpochStateCell`上线性化。secret 只能由 HTTP auth middleware 和 session
configuration builder 读取。

`PythonSchemaValidationBridgeLease` 只在 `CompiledActSchema.validation_mode == Hybrid` 时创建。
native HTTP worker 通过它把 bounded `CustomValidationJob` 发到当前 `_ActCall` 的 Python loop；
每个 dispatch 都携带 exact operation、arm、bridge generation 和 schema path。bridge state、slot
tombstone 与 caller outcome 在同一 handoff critical section 线性化；supervisor continuation 只接收
closed bridge/tombstone，不接收调用 Python 的能力。

核心要求：

- readiness 和 exact turn completion 都是 lossless single-assignment channel，不使用
  lossy observation stream 作为控制面。
- worker 独占 mutable ACP connection/session state。
- public `act()` path 没有 command FIFO；只有 caller admission 的 owner 能等待 availability，
  只有 session-turn lease 的 owner 能提交 exact turn。worker handle 还接受 lifecycle control，
  cancellation 则通过预留 continuation 在 worker 内完成 ownership handoff。
- Runtime 不建立 per-turn update collection；普通 updates 分类后丢弃，correctness evidence
  保存在独立的 bounded control state 中。
- provider-specific metadata 只有被 pinned adapter 归一化为 settlement evidence 时才保留，
  不泄漏进 Actor result contract 或 authority。
- prompt renderer、native validator、custom validation walker 和 PyO3 materializer 共享
  `CompiledActSchema`。built-in constraint 不能在 Python 或 provider adapter 中维护第二份逻辑；
  custom `render_prompt()`/`validate()` 是用户明确选择的 extension，其一致性不由 Troupe 推断。
- adapter 必须输出 normalized settlement evidence；核心层不能从 error text、最后一条
  update 或裸 `stopReason` 猜 session health。
- reverse request handler 不能调用 Python、等待 UI 或进入 Actor mailbox；它使用 pinned
  adapter 的确定性规则立即应答。writer 只等待 flush、transport error、turn cancellation
  或显式 close，不设置 frame-write timeout。需要在应答后 terminalize session 的 request 先登记
  deferred protocol evidence；response 后同一 raw batch 中的 request 必须在 prompt awaiter 被唤醒
  前登记。pending response flush 归零前，turn settlement 保持 pending：既不能用该 evidence 提前
  提交 `Broken`，也不能把它暂时当作 `false` 而发布成功。归零后 boundary evidence 是该 turn 原子
  settlement 的 terminal input，适用于 error 和 successful prompt response；异步 terminal task 只做
  相同 immutable failure 的幂等提交，不能成为正确性的竞态前提。若 transport 先失败，则现有
  terminal-fault latch 直接收口，不再等待不可能完成的 flush。

## 14. 错误契约与 V1 诊断边界

Public exception 层级为：

```text
AgentError(RuntimeError)
├── AgentSessionBusyError
├── AgentSessionError
│   ├── AgentSessionStartError
│   │   └── AgentAuthenticationRequiredError
│   └── AgentSessionBrokenError
└── AgentTurnError
    └── AgentResultError
        └── AgentResultMissingError
```

schema programming API 另有不属于 `AgentError` 的本地类型：

```text
act_schema.ValueRejected(ValueError)       # callback 内部的受控 invalid-value signal
act_schema.SchemaCallbackError(RuntimeError)  # render/validate callback 自身失败
```

`AgentError` hierarchy 从顶层 `troupe` 导出，`__module__ == "troupe"`；schema programming
types 只从 `troupe.act_schema` 导出，`__module__ == "troupe.act_schema"`。agent exception 类型
表示稳定的 fault domain；
每个 `AgentError` 都有一个 public `code: str`，其值来自该 concrete class 的 closed、
snake_case Troupe vocabulary。provider raw stop reason、error text、stderr 和 JSON-RPC payload
不能进入 public field、message、`args` 或 `__cause__`；完成当前 protocol classification 与
normalized fault mapping 后直接释放。
`str(error)` 与 `args == (message,)` 用于人类阅读，message 文案不作为兼容性 API。

映射固定为：

| 类别 | public exception | Session 后续语义 |
|---|---|---|
| 非法 cued authority | 现有 `CueContextError` | 不变 |
| 非法 script/schema descriptor/placement/bounds | `TypeError` / `ValueError` | 不变，未提交 |
| custom `render_prompt()` exception/非法 return/超限 | `act_schema.SchemaCallbackError(phase="render_prompt")` | 不变，未提交 |
| 同 Actor caller admission 冲突 | `AgentSessionBusyError(code="concurrent_act")` | 不变；不会形成 FIFO |
| Opening 需要认证 | `AgentAuthenticationRequiredError(code="authentication_required")` | terminal `AuthRequired` |
| deterministic Opening failure | `AgentSessionStartError` | terminal `StartFailed` |
| post-Ready auth/transport/process/protocol/result-channel loss | `AgentSessionBrokenError` | terminal `Broken` |
| authoritative refusal/limit/request error 或 unsolicited remote cancel | `AgentTurnError` | session 未被本错误判为 Broken；下一轮可直接继续或等待既有 settlement |
| 有 invalid result attempt 且终局仍无 accepted result，或达到 invalid-call 上限 | `AgentResultError` | caller 可先返回；supervisor settlement 期间可暂为 `Cancelling` |
| custom `validate()` unexpected exception/非法 return | `act_schema.SchemaCallbackError(phase="validate")` | caller failure handoff 后可暂为 `Cancelling`；settlement 决定 `Ready/Broken` |
| authoritative `end_turn` 且从未调用 result tool | `AgentResultMissingError` | `Ready` |
| 已验证 result 的 PyO3 materialization 失败 | 原生 Python runtime exception，不包装为 `AgentError` | `Ready`；不是 agent failure |
| caller/parent 取消 | 原生 `asyncio.CancelledError` | 本地 handoff 后返回；后台可为 Cancelling/Ready/Broken |
| cleanup 失败 | 不从 `act()` 抛新异常 | 进入 Production lifecycle aggregate |

第一版 Runtime 只抛下列 concrete code；新增 code 属于 public API 变更：

- `AgentSessionBusyError`: `concurrent_act`；
- `AgentSessionStartError`: `launcher_unavailable`、`preparation_failed`、`spawn_failed`、
  `protocol_incompatible`、`configuration_invalid`、`result_channel_unavailable`、
  `resource_limit`、`crash_loop`；
- `AgentAuthenticationRequiredError`: `authentication_required`；
- `AgentSessionBrokenError`: `transport_lost`、`process_exited`、`protocol_violation`、
  `uncertain_settlement`、`authentication_lost`、`result_channel_lost`、`resource_limit`；
- `AgentTurnError`: `refused`、`max_tokens`、`max_turn_requests`、`request_failed`、
  `remote_cancelled`；
- `AgentResultError`: `invalid_result`、`too_many_invalid_results`；
- `AgentResultMissingError`: `missing_result`。

`AgentSessionStartError` 另有 closed `phase: str`：`preparation`、`spawn`、`initialize`、
`session_new`、`configure` 或 `mcp_ready`。result validation detail 是业务
correctness evidence，不属于 diagnostic metadata：

```python
class AgentResultIssue:
    path: str      # JSON Pointer
    code: str      # closed normalized validation code
    message: str   # human-readable, not stable API

class AgentResultError(AgentTurnError):
    issues: tuple[AgentResultIssue, ...]  # declaration/path order, at most 16
    invalid_calls: int
    details_truncated: bool
```

`AgentResultIssue` 是 Runtime 构造的 immutable、final value object，也从顶层 `troupe`
导出。其第一版 closed `code` 固定为
`type_mismatch`、`missing_field`、`extra_field`、
`out_of_range`、`not_in_choices`、`length_limit`、`item_limit`、`custom_validation` 和
`resource_limit`。`AgentResultMissingError`
继承同一 result fault domain，但其 `issues == ()`、`invalid_calls == 0` 且
`details_truncated is False`。

`ValueRejected` 只允许从 active custom `validate()` 传播到 bridge，永不直接作为 `act()`
caller outcome。Runtime 负责添加当前 JSON Pointer path、固定 `custom_validation` code 和 bounded
message。`SchemaCallbackError` 有 immutable `phase` 与 `path`，message 不稳定；它保留原 user
exception 为 `__cause__`，因为这是调用方代码而不是必须脱敏的 provider payload。callback fault
若输给 concurrent caller cancellation，则不再向已取消 caller 交付，也不改变 handoff outcome。

不提供 `retryable`、`session_reusable` 或 provider-specific field。flow 根据异常类型和 code
决定策略；下一次 `act()` 本身负责等待 pending settlement 或报告 cached terminal session
failure。`AuthRequired`、`StartFailed`、`Broken` 保存 immutable failure snapshot，但每次
观察都创建新的等价 exception instance，不能复用带 traceback 的旧 Python exception。

V1 不定义 diagnostics、trace、usage collection、event exporter 或持久化 audit contract。
Runtime 只保留完成当前 correctness、public error 和 lifecycle transition 所必需的 bounded
state，普通 updates 分类后丢弃。完整诊断体系与持久化体系如有需要，后续独立设计；它们不能
反向改变本设计的 result、retry、cancel、settlement 或 cleanup 语义。

## 15. 实现与验证顺序

### 15.1 Deterministic ACP vertical slice

- 引入官方 Rust SDK、in-process loopback HTTP MCP service 和 mock ACP agent/client。
- 覆盖 background open、shared readiness、两个 contextual turns、fail-fast caller admission、
  transient Opening retry、crash-loop threshold、terminal auth、自动 permission/question
  响应、authoritative/uncertain settlement、cancel race、supervisor handoff、event-driven
  settlement、capability destruction/drop、普通 update 丢弃和无 per-turn history。
- 覆盖 Opening request、partial ACP frame、HTTP body、MCP readiness 和 supervisor cancel
  settlement 在无 event 时保持 pending，证明 elapsed time 不产生 failure、retry、`Broken`
  或 kill；同时证明已取消 `_ActCall`、Cue drain 和 mailbox slot 不再等待 provider。
- 覆盖 handoff 后 supervisor 继续持有 session-turn lease/result route；未观察到 terminal
  时恰好发送一次 cancel，已经 authoritative terminal 时不发送；继续处理 pending reverse
  request 和 tail update。下一次 admitted `act()` 等 availability 且不提交 prompt，settlement
  后才复用同一 session，另一个并发 caller 仍 fail-fast。
- 覆盖 Actor capability destruction/Production shutdown 抢先结束 pending operation、立即 destructive
  teardown、并发等待所有 process 的 OS-confirmed exit/reap。
- 覆盖 `127.0.0.1:0` bind、listener lifetime、per-`session/new`-invocation capability rotation、跨 Actor
  route isolation、MCP readiness、invalid submission correction、first-valid-wins、missing
  result、accepted-result/non-end-turn 和 listener loss。
- 用barrier覆盖MCP-ready在`session/new` response前、configuration开始前、configuration RPC之间和
  Configuration-ready后四种Opening interleaving；每种都只在同generation两latch join后Ready，且此前
  零prompt。auth-required分支覆盖旧MCP-ready与route revoke两种竞态顺序，并证明它不发送
  `authenticate`、不重试 `session/new`，旧 route 在 terminal publication 前完成撤销。
- 覆盖 initialize HTTP MCP capability absent/false、typed `McpServer::Http` discriminator、
  32-byte bearer header唯一允许出现的位置、missing/wrong header不修改route，以及retry/destruction
  后旧capability失效。它们只验证routing correctness，不作security assertion。
- 用同一production decoder分别覆盖 registry 中每个 exact MCP revision 的 initialize -> initialized -> tools/list、
  POST Accept/Content-Type/version header、JSON response/202 notification、GET/DELETE 405、无
  `MCP-Session-Id`、wrong revision/Origin/lifecycle拒绝，以及`2026-07-28` stateless first-call不被接受。
- 在workspace pathname revalidation之后、process `fchdir`或`session/new`实际消费之前插入
  rename-and-replace barrier，证明两者仍解析到held directory handle的old inode，随后Opening
  checkpoint观察mismatch并cleanup，绝不使用同名replacement。
- 对每个 `ResourceLimitsV1` 数值覆盖 `N-1`、`N`、`N+1`，包括 chunked HTTP body、深层
  JSON、node amplification、第 9 次 invalid call 的 `Rejected` 线性化，以及超限后 slot/
  session health 的精确状态。
- HTTP connection 上限用 fake permit/accounting 测试 65,536 的全局边界、无 per-Actor
  quota 和拒绝新连接语义；普通测试不实际创建 65,536 个 socket。
- 覆盖 assistant text、plan/status、usage、tool progress/output 分类后丢弃，证明 Runtime 不保留
  per-turn history，同时 result、settlement、reverse-request 和 lifecycle correctness evidence
  不丢失。
- 覆盖Ready后的完整config snapshot：exact selection update继续运行；model/effort drift、present
  null和malformed known option在result commit前后两种顺序分别得到当前turn Broken或已提交结果+
  later Broken，且不reconfigure/reopen。
- 在Opening、idle Ready、Active、Cancelling分别用barrier固定process-exit先于EOF和EOF先于
  process-exit；`TerminalFaultLatchV1`每次只有一个public outcome、route revoke、cleanup lease与reap，
  loser只进入bounded corroboration且Cancelling caller的`CancelledError`不被覆盖。
- 覆盖每种 DSL node、nested object/list、optional 与 nullable 区分、extra/missing field、
  required description、typed choices、boundary value、strict no-coercion、declaration-order
  materialization、invalid graph 和 built-in prompt/validator golden parity。
- 覆盖 public `SchemaValue` 直接/间接 subclass、concrete built-in subclass rejection、全部
  `json_kind` native precheck、sync/async `validate()`、custom prompt freeze、defensive copy、
  `ValueRejected` correction、unexpected callback fault、non-`None` return、同 slot serialization、
  cross-Actor concurrency、repeated/list-item invocation 和 callback/cancel/slot CAS 竞态。
- 覆盖所有 public exception 的顶层 export、继承关系、closed code、start phase、bounded
  `AgentResultIssue`、raw provider data 不泄漏、cached terminal snapshot 每次生成新 instance，
  以及 `TypeError`/`ValueError`/`CueContextError`/`CancelledError` 不包装。
- 用 mock 明确复现每个 state transition 和竞态，不依赖真实 provider 费用或速度。

### 15.2 Actor runtime integration

- 在 `ActorCapability` 加 session slot，在 Production native service 加 supervisor、
  固定 `ResourceLimitsV1` 和共享 `ResultMcpService`。
- 从 `Actor.make_effect()` 的现有路径抽取 current-Actor cued authority helper。
- 实现同步 preflight + one-shot `_ActCall`。
- 导出 agent exception hierarchy、immutable `AgentResultIssue`、public `SchemaValue`、
  `ValueRejected` 与 `SchemaCallbackError`，同步更新 native
  module、顶层 package、type stub、wheel verifier 和 public API identity tests。
- 把 turn 注册为 CuedScope structured child operation，并实现 preallocated supervisor
  continuation、caller admission/session-turn lease 分离和 lossless availability barrier。
- 增加 capability Drop cleanup lease 和 fork safety。

### 15.3 Result contract

- 固定 prompt template version。
- 实现 immutable built-in descriptors、required descriptions、typed choices、`ObjectValue`、
  public custom subclass protocol、`CompiledActSchema`、`ValidatedActValue`、prompt renderer、
  native-only fast path、hybrid Python validation bridge 和 final PyO3 materializer。
- 实现静态 MCP tool、per-act `ResultSlot` 和 typed errors；MCP input schema 只描述固定
  `{value: JSON object}` envelope，不承担 per-act field constraint。
- 加入 HTTP/body/value/schema-callback bounds、validation feedback 和文档；不实现 assistant
  final-text parser 或 SQLite result path。

### 15.4 Provider acceptance

普通单元测试使用 mock ACP process 和 deterministic barriers，不启动真实 provider。另提供
显式 opt-in 的 Codex、Claude、Kimi live acceptance；每个内置 adapter 至少覆盖：

1. initialize/new 与 capability snapshot。
2. 两轮上下文记忆。
3. project instructions、tools 和 workspace 行为。
4. adapter-pinned exact MCP initialize/initialized/tools-list与`McpTransportProfileV1` HTTP
   conformance、代表性的 nested/bounded contract result submit、tool error feedback 和不调用 tool。
5. cancel before-send、model/tool/permission 中取消、tail update、normal-completion race、
   cancel 后同 session 下一轮，以及 synthetic cancel 的 typed-evidence classification。
6. 每个 provider 都用未预登录的 isolated real child 验证 typed terminal auth failure：Opening
   明确报告认证缺失时为 `authentication_required`，只在首个 prompt 惰性发现时为
   `authentication_lost`；同时验证 authoritative local completion、OS-confirmed cleanup 以及
   zero browser/terminal/device-login/permission/elicitation；
   deterministic mock另穷尽归一化分支。
7. 对每个adapter rule用deterministic mock分别隔离idle/in-flight process exit、
   transport-only EOF和malformed/late event，并在Opening、idle、Active、Cancelling覆盖process/EOF
   两种到达顺序与first-winner idempotence；用real provider process验证Actor/Production destruction的
   whole-process-tree cleanup。
8. MCP request/value/depth/invalid-call limit 与 malformed tool arguments。
9. provider-specific terminal/error 到 Troupe closed exception code 的完整映射，确保 raw
   stop reason、stderr 和 payload 不进入 public exception。
10. schema/result shared corpus 在 Rust built-in validator、prompt renderer 和 Python materializer
    间保持一致；每个 provider 另覆盖一次 choices correction 和一次 custom `ValueRejected` correction，
    证明 static MCP tool 不依赖 provider-specific structured output。

Codex 额外覆盖 plan/review event；Claude 覆盖 disabled elicitation、permission、Claude Code
preset、user/project/local settings 与 hooks，且启动路径不依赖交互 shell profile；Kimi 覆盖
typed notification、本地 shell event 和 permission-overloaded question。三者都要证明 exact
internal mode 在首轮 prompt 前生效、session-scoped HTTP MCP 在首轮 prompt 前 ready，同时接受
MCP 在 `session/new` response 前 ready 的合法 eager path。assistant final event 只验证协议分类与
丢弃路径，不参与 result selection。

mock 和 live tests 直接断言 public outcome、session health、route cleanup 与 process cleanup；
测试成功或失败本身就是本阶段的验收结果，不生成额外的 repository/package 产物。


## 16. 非目标

第一版不包括：

- 任何 Troupe-owned timeout/watchdog，包括 application/turn、inactivity、Opening handshake、
  ACP/HTTP I/O、cancel settlement、graceful close、process cleanup 与 Production shutdown。
- 多个 Actor 共享一个 conversation session。
- 一个 Actor 同时运行多个 turns。
- Actor 内 turn FIFO。
- 每次 `act()` 新建 session。
- 失败后静默创建空白 session。
- ACP 到 PTY 的自动 fallback。
- 让 schema 充当 tool/security policy。
- 把 public `output_schema` 解释为 JSON Schema，或承诺 JSON Schema dialect/vocabulary。
- custom validator 对值的 coercion/replacement，或 Troupe 对 callback purity、termination、
  external side effect、thread safety、prompt/validator consistency 的保证。
- built-in DSL v1 中的 general union、dynamic-key map、tuple、recursive reference、
  generic `AnyValue`、default injection 或 extra-field opt-out。
- 从 assistant 最终文本、Markdown 或 update stream 猜测/解析业务 result。
- SQLite、文件或其他由 agent 直接写入的 Runtime result storage。
- 手工扫描端口范围、固定 public MCP port 或每 Actor listener/socket 文件。
- 在已经创建的 ACP session 中 hot-add/hot-remove MCP server。
- 依赖动态 `tools/list_changed` 才能保证每轮 schema correctness。
- 把 loopback MCP endpoint 用于远程 ACP agent；第一版 agent process 与 Runtime 在同一
  network namespace。
- 跨 Troupe process 的 Actor/session persistence、自动恢复，或只恢复 agent conversation
  而不恢复 Actor/turn state 的 partial recovery。
- 完整复刻 provider TUI。
- 在后台 `act()` 中启动交互登录 UI。
- 在后台 `act()` 中等待人工 permission、问题回答或 plan approval。
- Troupe-provided sandbox 或跨 provider security isolation。
- 把 `workspace`、ACP permission 或 `output_schema` 描述成安全边界。

## 17. 资料依据与版本快照

本文的 ACP/provider 快照来自：

- [Agent Client Protocol specification](https://github.com/agentclientprotocol/agent-client-protocol)
- [ACP session setup and `mcpServers`](https://agentclientprotocol.com/protocol/v1/session-setup)
- [ACP session config options](https://agentclientprotocol.com/protocol/v1/session-config-options)
- [ACP legacy session modes](https://agentclientprotocol.com/protocol/v1/session-modes)
- [Official ACP Rust SDK](https://github.com/agentclientprotocol/rust-sdk)
- [MCP 2025-06-18 lifecycle](https://modelcontextprotocol.io/specification/2025-06-18/basic/lifecycle)
- [MCP 2025-11-25 lifecycle](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle)
- [MCP 2025-11-25 Streamable HTTP transport](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports)
- [MCP tools specification](https://modelcontextprotocol.io/specification/2025-11-25/server/tools)
- [MCP 2026-07-28 lifecycle removal note](https://blog.modelcontextprotocol.io/posts/2026-07-28/)
- [codex-acp 1.1.9 session MCP mapping](https://github.com/agentclientprotocol/codex-acp/blob/v1.1.9/src/CodexAcpClient.ts#L382-L390)
- [codex-acp 1.1.9 MCP E2E](https://github.com/agentclientprotocol/codex-acp/blob/v1.1.9/src/__tests__/CodexACPAgent/e2e/acp-e2e-mcp-approval.test.ts#L66-L90)
- [Codex 0.145.0 MCP client revision pin](https://github.com/openai/codex/blob/rust-v0.145.0/codex-rs/codex-mcp/src/rmcp_client.rs#L915-L932)
- [claude-agent-acp 0.64.2 eager session initialization](https://github.com/agentclientprotocol/claude-agent-acp/blob/v0.64.2/src/acp-agent.ts#L5145-L5421)
- [Kimi Code 0.31.1](https://github.com/MoonshotAI/kimi-code/tree/%40moonshot-ai%2Fkimi-code%400.31.1)

具体版本只说明 2026-08-03 的设计调研对象，不是当前或未来 package 的版本承诺。Runtime
行为由第 12.2 节的 private launch registry 和第 15.4 节的 acceptance suite 约束。
