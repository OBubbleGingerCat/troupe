// Troupe-owned ACP stable-v1 bridge for Pi's native RPC mode.
//
// This file is embedded into the Rust runtime and staged with mode 0600 at
// launch.  stdout is an ACP JSONL transport: diagnostics must never be printed
// here.  Pi's stderr is intentionally drained and only a bounded generic error
// is exposed to the ACP client.

import { spawn } from "node:child_process";
import process from "node:process";
import { StringDecoder } from "node:string_decoder";

const FRAME_MAX = 16 * 1024 * 1024;
const STDERR_MAX = 256 * 1024;
const BRIDGE_VERSION = "0.1.0";
const PI_RPC_STARTUP_TIMEOUT_MS = 15_000;
const PI_RPC_PROMPT_ACCEPT_TIMEOUT_MS = 30_000;
const PI_RPC_ABORT_TIMEOUT_MS = 5_000;
const PI_RPC_SETTLEMENT_TIMEOUT_MS = 10 * 60 * 1_000;
const TOOL_INPUT_MAX = 1024 * 1024;
const PI_ERROR_TEXT_MAX = 4 * 1024;
const ALLOWED_MODELS = new Set(["deepseek-flash", "deepseek-v4-pro"]);
const ALLOWED_EFFORTS = new Set(["low", "medium", "high", "xhigh", "max"]);

function parseArgs(argv) {
  const result = {};
  for (let index = 0; index < argv.length; index += 1) {
    const key = argv[index];
    if (!key.startsWith("--")) continue;
    const name = key.slice(2);
    const next = argv[index + 1];
    if (next !== undefined && !next.startsWith("--")) {
      result[name] = next;
      index += 1;
    } else {
      result[name] = true;
    }
  }
  return result;
}

const args = parseArgs(process.argv.slice(2));
const piCommand = typeof args["pi-command"] === "string" ? args["pi-command"] : null;
const extension = typeof args.extension === "string" ? args.extension : null;
const selectedModel = typeof args.model === "string" ? args.model : null;
const selectedThinking = typeof args.thinking === "string" ? args.thinking : null;
const requestedEffort = typeof args["requested-effort"] === "string" ? args["requested-effort"] : null;

if (!piCommand || !extension || !selectedModel) {
  process.stderr.write("Troupe Pi bridge arguments are incomplete\n");
  process.exit(78);
}
if (!ALLOWED_MODELS.has(selectedModel)) {
  process.stderr.write("Troupe Pi bridge model is not allowlisted\n");
  process.exit(78);
}
if (requestedEffort !== null && !ALLOWED_EFFORTS.has(requestedEffort)) {
  process.stderr.write("Troupe Pi bridge effort is not allowlisted\n");
  process.exit(78);
}

let sessionId = null;
let acpInitialized = false;
let routeServer = null;
let child = null;
let childBuffer = "";
const piDecoder = new StringDecoder("utf8");
const acpDecoder = new StringDecoder("utf8");
let nextPiRequest = 1;
const pendingPi = new Map();
let activePrompt = null;
let shuttingDown = false;
let broken = false;
let cancelTimer = null;
let settlementTimer = null;
let lastProviderError = null;
let lastStopReason = "end_turn";
let currentMessageId = null;
const toolInputs = new Map();
const toolInputBuffers = new Map();
let stderrBytes = 0;
let configurationStep = 0;
let config = {
  mode: "default",
  model: selectedModel,
  ...(requestedEffort === null ? {} : { thinking: requestedEffort }),
};

function writeAcp(message) {
  if (shuttingDown) return;
  const encoded = JSON.stringify(message);
  if (Buffer.byteLength(encoded, "utf8") > FRAME_MAX) {
    process.stderr.write("Troupe Pi bridge produced an oversized ACP frame\n");
    failProtocol("acp_frame_limit");
    return;
  }
  process.stdout.write(`${encoded}\n`);
}

function response(id, result) {
  writeAcp({ jsonrpc: "2.0", id, result });
}

function errorResponse(id, code, message, data) {
  const error = { code, message };
  if (data !== undefined) error.data = data;
  writeAcp({ jsonrpc: "2.0", id, error });
}

function setProviderError(kind, reason) {
  const priority = {
    prompt: 10,
    extension: 20,
    process: 30,
    provider: 40,
    auth: 50,
    protocol: 60,
  };
  if (
    lastProviderError === null ||
    (priority[kind] ?? 0) > (priority[lastProviderError.kind] ?? 0)
  ) {
    lastProviderError = { kind, reason };
  }
}

function piErrorText(value) {
  if (typeof value === "string") return value.slice(0, PI_ERROR_TEXT_MAX);
  if (value === null || value === undefined) return "";
  try {
    return String(JSON.stringify(value)).slice(0, PI_ERROR_TEXT_MAX);
  } catch {
    return "";
  }
}

// Pi's RPC error strings are provider-specific and intentionally not copied
// into ACP data. Only these finite categories cross the Troupe boundary.
function classifyPiError(value, defaultKind = "prompt", defaultReason = "prompt_rejected") {
  const text = piErrorText(value).toLowerCase();
  const normalized = text.replace(/[\s_-]+/g, " ");
  if (
    /api key|authentication|unauthori[sz]ed|invalid credential|credential|login required|\b401\b|\b403\b/.test(
      normalized,
    )
  ) {
    return { kind: "auth", reason: "authentication_failed" };
  }
  if (
    /rate limit|too many requests|overloaded|capacity|quota|billing|payment|\b429\b|\b5\d\d\b|server error|service unavailable|temporarily unavailable|model .*unavailable|model .*not found|provider error/.test(
      normalized,
    )
  ) {
    return { kind: "provider", reason: "provider_request_failed" };
  }
  return { kind: defaultKind, reason: defaultReason };
}

function setProviderErrorFromEvent(value, fallbackReason) {
  const classified = classifyPiError(value, "provider", fallbackReason);
  setProviderError(classified.kind, classified.reason);
}

function clearCancelTimer() {
  if (cancelTimer !== null) {
    clearTimeout(cancelTimer);
    cancelTimer = null;
  }
}

function clearSettlementTimer() {
  if (settlementTimer !== null) {
    clearTimeout(settlementTimer);
    settlementTimer = null;
  }
}

function armCancelTimer() {
  clearCancelTimer();
  cancelTimer = setTimeout(() => {
    if (activePrompt?.cancelled) failProtocol("cancel_timeout");
  }, PI_RPC_ABORT_TIMEOUT_MS);
}

function armSettlementTimer(prompt) {
  clearSettlementTimer();
  settlementTimer = setTimeout(() => {
    if (activePrompt === prompt && prompt.accepted) failProtocol("settlement_timeout");
  }, PI_RPC_SETTLEMENT_TIMEOUT_MS);
}

function notification(method, params) {
  writeAcp({ jsonrpc: "2.0", method, params });
}

function sessionUpdate(update) {
  if (!sessionId) return;
  notification("session/update", { sessionId, update });
}

function configOptions() {
  const options = [
    {
      id: "mode",
      name: "Mode",
      category: "mode",
      type: "select",
      currentValue: config.mode,
      options: [{ value: "default", name: "Default" }],
    },
    {
      id: "model",
      name: "Model",
      category: "model",
      type: "select",
      currentValue: config.model,
      options: [
        { value: "deepseek-flash", name: "DeepSeek Flash" },
        { value: "deepseek-v4-pro", name: "DeepSeek V4 Pro" },
      ],
    },
  ];
  if (requestedEffort !== null) {
    options.push({
      id: "thinking",
      name: "Thinking",
      category: "thought_level",
      type: "select",
      currentValue: config.thinking,
      options: [
        { value: "low", name: "low" },
        { value: "medium", name: "medium" },
        { value: "high", name: "high" },
        { value: "xhigh", name: "xhigh" },
        { value: "max", name: "max" },
      ],
    });
  }
  return options;
}

function routeCredentials(server) {
  const rawHeaders = server?.headers;
  const headers = Array.isArray(rawHeaders)
    ? rawHeaders
    : rawHeaders && typeof rawHeaders === "object"
      ? Object.entries(rawHeaders).map(([name, value]) => ({ name, value }))
      : [];
  const authorization = headers.find((header) => header?.name?.toLowerCase() === "authorization")?.value;
  if (typeof server?.url !== "string" || typeof authorization !== "string") {
    throw new Error("result route credentials are unavailable");
  }
  return { endpoint: server.url, authorization };
}

function firstMcpServer(value) {
  if (Array.isArray(value)) return value[0] ?? null;
  if (value && typeof value === "object") {
    if (value.type === "http" || typeof value.url === "string") return value;
    if (value.http && typeof value.http === "object") return value.http;
    const first = Object.values(value).find(
      (candidate) => candidate && typeof candidate === "object" && (candidate.type === "http" || typeof candidate.url === "string"),
    );
    return first ?? null;
  }
  return null;
}

function sendPi(command, timeoutMs = 0) {
  if (!child || !child.stdin.writable) {
    return Promise.reject(new Error("Pi RPC process is unavailable"));
  }
  const id = `troupe-${nextPiRequest++}`;
  const line = JSON.stringify({ id, ...command });
  if (Buffer.byteLength(line, "utf8") > FRAME_MAX) {
    return Promise.reject(new Error("Pi RPC command is oversized"));
  }
  return new Promise((resolve, reject) => {
    let timer = null;
    const settle = (callback, value) => {
      if (timer !== null) clearTimeout(timer);
      callback(value);
    };
    const pending = {
      resolve: (record) => settle(resolve, record),
      reject: (error) => settle(reject, error),
      command: command.type,
    };
    pendingPi.set(id, pending);
    if (timeoutMs > 0) {
      timer = setTimeout(() => {
        if (pendingPi.delete(id)) {
          pending.reject(new Error("Pi RPC command timed out"));
        }
      }, timeoutMs);
    }
    child.stdin.write(`${line}\n`, (writeError) => {
      if (writeError) {
        if (pendingPi.delete(id)) pending.reject(new Error("Pi RPC write failed"));
      }
    });
  });
}

function rejectPendingPi(error) {
  for (const pending of pendingPi.values()) pending.reject(error);
  pendingPi.clear();
}

function emitText(kind, text) {
  if (typeof text !== "string" || text.length === 0) return;
  const update = {
    sessionUpdate: kind,
    content: { type: "text", text },
  };
  if (typeof currentMessageId === "string" && currentMessageId.length > 0) {
    update.messageId = currentMessageId;
  }
  sessionUpdate(update);
}

function emitToolStart(event) {
  if (typeof event.toolCallId !== "string") return;
  const candidate = event.args && typeof event.args === "object" && !Array.isArray(event.args)
    ? event.args
    : toolInputs.get(event.toolCallId);
  const args = candidate && typeof candidate === "object" && !Array.isArray(candidate)
    ? candidate
    : {};
  sessionUpdate({
    sessionUpdate: "tool_call",
    toolCallId: event.toolCallId,
    title: typeof event.toolName === "string" ? event.toolName : "Pi tool",
    kind: "other",
    status: "in_progress",
    rawInput: args,
  });
}

function emitToolEnd(event) {
  if (typeof event.toolCallId !== "string") return;
  sessionUpdate({
    sessionUpdate: "tool_call_update",
    toolCallId: event.toolCallId,
    status: event.isError ? "failed" : "completed",
    rawOutput: event.result ?? null,
  });
  toolInputs.delete(event.toolCallId);
  toolInputBuffers.delete(event.toolCallId);
}

function rememberToolCallDelta(event) {
  const id = event?.id ?? event?.toolCall?.id;
  if (typeof id !== "string" || id.length === 0) return;
  let value = toolInputBuffers.get(id) ?? "";
  if (event.type === "toolcall_start") {
    value = "";
  } else if (typeof event.delta === "string") {
    value += event.delta;
  } else if (event.type === "toolcall_end" && event.toolCall) {
    const args = event.toolCall.arguments ?? event.toolCall.args;
    if (typeof args === "string") value = args;
    else if (args !== undefined) {
      try {
        value = JSON.stringify(args);
      } catch {
        value = "";
      }
    }
  }
  if (Buffer.byteLength(value, "utf8") > TOOL_INPUT_MAX) {
    toolInputs.delete(id);
    toolInputBuffers.delete(id);
    return;
  }
  toolInputBuffers.set(id, value);
  try {
    toolInputs.set(id, JSON.parse(value));
  } catch {
    // Do not expose a partial or malformed payload as a canonical ACP
    // rawInput value.  The tool execution start event will use an empty
    // object until Pi emits a complete argument object.
    toolInputs.delete(id);
  }
}

function stopReasonForPi(reason) {
  switch (reason) {
    case "length":
      return "max_tokens";
    case "aborted":
      // Pi can abort a turn because of an internal/provider failure as well
      // as an explicit ACP cancellation.  Only the latter is an ACP
      // cancellation boundary; an unsolicited abort must remain an error.
      return activePrompt?.cancelled ? "cancelled" : "end_turn";
    case "error":
      return "end_turn";
    default:
      return "end_turn";
  }
}

function settlePrompt(stopReason = "end_turn", force = false) {
  if (!activePrompt) return;
  const prompt = activePrompt;
  // Pi documents the prompt response as an acceptance/preflight boundary.
  // Keep an early agent_settled event pending until that boundary is observed;
  // failures before acceptance use force=true below.
  if (!force && !prompt.accepted) {
    prompt.pendingStopReason = stopReason;
    return;
  }
  activePrompt = null;
  clearCancelTimer();
  clearSettlementTimer();
  if (prompt.cancelled && !prompt.cancelFailure) {
    response(prompt.id, { stopReason: "cancelled" });
  } else if (prompt.cancelled) {
    errorResponse(prompt.id, -32001, "Pi bridge could not cancel the turn", {
      piErrorKind: "cancel",
      provider: "deepseek",
      model: selectedModel,
      reason: prompt.cancelFailure,
    });
  } else if (lastProviderError) {
    const code = lastProviderError.kind === "auth"
      ? -32000
      : lastProviderError.kind === "provider"
        ? -32603
        : -32001;
    const message = lastProviderError.kind === "auth"
      ? "Pi authentication failed"
      : lastProviderError.kind === "provider"
        ? "Pi provider turn failed"
        : "Pi bridge turn failed";
    errorResponse(prompt.id, code, message, {
      piErrorKind: lastProviderError.kind,
      provider: "deepseek",
      model: selectedModel,
      reason: lastProviderError.reason,
    });
  } else {
    response(prompt.id, { stopReason });
  }
  lastProviderError = null;
  lastStopReason = "end_turn";
  currentMessageId = null;
  toolInputs.clear();
  toolInputBuffers.clear();
}

function failProtocol(reason) {
  broken = true;
  rejectPendingPi(new Error("Pi RPC protocol failure"));
  if (activePrompt) {
    if (activePrompt.cancelled) activePrompt.cancelFailure = reason;
    setProviderError("protocol", reason);
    settlePrompt("end_turn", true);
  }
  if (child && child.exitCode === null) child.kill();
}

function handlePiRecord(record) {
  if (record?.type === "response") {
    const pending = pendingPi.get(record.id);
    if (!pending) return;
    pendingPi.delete(record.id);
    if (record.success === false) {
      const classified = classifyPiError(record.error);
      const error = new Error("Pi RPC command was rejected");
      error.piErrorKind = classified.kind;
      error.piErrorReason = classified.reason;
      pending.reject(error);
    } else {
      pending.resolve(record);
    }
    return;
  }
  // Events are scoped to the active ACP prompt. Pi normally emits
  // agent_settled as the final event, but a child or extension can still flush
  // a late message while the bridge is moving to the next turn. Never let that
  // content cross the Troupe turn boundary.
  if (!activePrompt) return;
  if (!record || typeof record.type !== "string") return;
  if (record.type === "message_start") {
    const id = record.message?.id;
    currentMessageId = typeof id === "string" ? id : null;
    return;
  }
  if (record.type === "message_update") {
    const event = record.assistantMessageEvent;
    if (event?.type === "text_delta") emitText("agent_message_chunk", event.delta);
    if (event?.type === "thinking_delta") emitText("agent_thought_chunk", event.delta);
    if (
      event?.type === "toolcall_start" ||
      event?.type === "toolcall_delta" ||
      event?.type === "toolcall_end"
    ) {
      rememberToolCallDelta(event);
    }
    return;
  }
  if (record.type === "tool_execution_start") {
    emitToolStart(record);
    return;
  }
  if (record.type === "tool_execution_update") {
    if (typeof record.toolCallId === "string") {
      sessionUpdate({
        sessionUpdate: "tool_call_update",
        toolCallId: record.toolCallId,
        rawOutput: record.partialResult ?? null,
      });
    }
    return;
  }
  if (record.type === "tool_execution_end") {
    emitToolEnd(record);
    return;
  }
  if (record.type === "extension_error") {
    setProviderError("extension", "extension_error");
    return;
  }
  if (record.type === "message_end") {
    const reason = record.message?.stopReason;
    lastStopReason = stopReasonForPi(reason);
    if (reason === "error") {
      setProviderErrorFromEvent(
        record.message?.errorMessage ?? record.message?.error ?? record.errorMessage,
        "provider_error",
      );
    }
    if (reason === "aborted" && !activePrompt?.cancelled) {
      setProviderErrorFromEvent(record.errorMessage, "provider_aborted");
    }
    currentMessageId = null;
    return;
  }
  if (record.type === "auto_retry_end" && record.success === false) {
    setProviderErrorFromEvent(
      record.finalError ?? record.errorMessage,
      "provider_retry_exhausted",
    );
    return;
  }
  if (record.type === "auto_retry_start") {
    // A failed message may be followed by a successful automatic retry.  Do
    // not leak the intermediate provider error into the final prompt result.
    if (lastProviderError?.kind === "provider") lastProviderError = null;
    return;
  }
  if (record.type === "auto_retry_end" && record.success === true) {
    if (lastProviderError?.kind === "provider") lastProviderError = null;
    return;
  }
  if (
    record.type === "compaction_end" &&
    record.aborted === false &&
    record.result == null
  ) {
    setProviderErrorFromEvent(record.errorMessage, "compaction_failed");
    return;
  }
  if (record.type === "summarization_retry_finished" && record.success === false) {
    setProviderErrorFromEvent(
      record.errorMessage,
      "summarization_retry_exhausted",
    );
    return;
  }
  if (record.type === "agent_settled") {
    settlePrompt(lastStopReason);
  }
}

function handlePiData(chunk) {
  childBuffer += piDecoder.write(chunk);
  while (true) {
    const newline = childBuffer.indexOf("\n");
    if (newline < 0) break;
    let line = childBuffer.slice(0, newline);
    childBuffer = childBuffer.slice(newline + 1);
    if (Buffer.byteLength(line, "utf8") > FRAME_MAX) {
      failProtocol("protocol_error");
      return;
    }
    if (line.endsWith("\r")) line = line.slice(0, -1);
    if (line.length === 0) continue;
    let record;
    try {
      record = JSON.parse(line);
    } catch {
      failProtocol("protocol_error");
      return;
    }
    handlePiRecord(record);
  }
  if (Buffer.byteLength(childBuffer, "utf8") > FRAME_MAX) failProtocol("protocol_error");
}

async function startPi() {
  const credentials = routeCredentials(routeServer);
  const childArgs = [
    "--mode", "rpc",
    "--offline",
    "--provider", "deepseek",
    "--model", selectedModel,
    "--no-session",
    "--no-extensions",
    "--no-builtin-tools",
    "--no-skills",
    "--no-prompt-templates",
    "--no-themes",
    "--no-context-files",
    "--no-approve",
    "-e", extension,
  ];
  if (selectedThinking) childArgs.push("--thinking", selectedThinking);
  const endpoint = new URL(credentials.endpoint);
  if (
    (endpoint.protocol !== "http:" && endpoint.protocol !== "https:") ||
    endpoint.hostname !== "127.0.0.1" ||
    endpoint.pathname !== "/mcp" ||
    endpoint.username !== "" ||
    endpoint.password !== "" ||
    endpoint.search !== "" ||
    endpoint.hash !== "" ||
    /[\r\n]/.test(credentials.authorization) ||
    credentials.authorization.length === 0 ||
    credentials.authorization.length > 4 * 1024 ||
    !credentials.authorization.startsWith("Bearer ") ||
    credentials.authorization.length <= "Bearer ".length
  ) {
    throw new Error("Pi result route is not a Troupe loopback route");
  }
  const childEnv = {
    ...process.env,
    TROUPE_RESULT_ENDPOINT: credentials.endpoint,
    TROUPE_RESULT_AUTHORIZATION: credentials.authorization,
    TROUPE_RESULT_REVISION: "2025-11-25",
    TROUPE_RESULT_ORIGIN: endpoint.origin,
  };
  child = spawn(piCommand, childArgs, {
    cwd: process.cwd(),
    env: childEnv,
    stdio: ["pipe", "pipe", "pipe"],
  });
  child.stdout.on("data", handlePiData);
  child.stdout.on("end", () => {
    const tail = piDecoder.end();
    if (tail) childBuffer += tail;
    if (!shuttingDown && childBuffer.trim() !== "") failProtocol("protocol_error");
  });
  child.stderr.on("data", (chunk) => {
    stderrBytes += chunk.length;
    if (stderrBytes > STDERR_MAX) failProtocol("stderr_limit");
  });
  child.on("error", () => {
    if (!shuttingDown) broken = true;
    rejectPendingPi(new Error("Pi RPC process failed to start"));
    if (activePrompt) {
      setProviderError("process", "process_error");
      if (activePrompt.cancelled) activePrompt.cancelFailure = "process_error";
      settlePrompt("end_turn", true);
    }
  });
  child.on("exit", () => {
    if (!shuttingDown) broken = true;
    rejectPendingPi(new Error("Pi RPC process exited"));
    if (activePrompt) {
      setProviderError("process", "process_exited");
      if (activePrompt.cancelled) activePrompt.cancelFailure = "process_exited";
      settlePrompt("end_turn", true);
    }
  });
  const state = await sendPi({ type: "get_state" }, PI_RPC_STARTUP_TIMEOUT_MS);
  if (state?.success !== true || state?.data?.model?.provider !== "deepseek" || state?.data?.model?.id !== selectedModel) {
    throw new Error("Pi RPC model preflight failed");
  }
}

async function handleAcp(request) {
  const method = request?.method;
  const id = request?.id;
  const params = request?.params ?? {};
  if (broken && method !== "initialize") {
    if (id !== undefined) errorResponse(id, -32001, "Pi bridge is broken");
    return;
  }
  if (method === "initialize") {
    if (acpInitialized) {
      errorResponse(id, -32603, "Pi bridge is already initialized");
      return;
    }
    acpInitialized = true;
    response(id, {
      protocolVersion: 1,
      agentCapabilities: {
        // Pi is deliberately started with --no-session.  Do not advertise
        // ACP session/load support that this bridge cannot implement.
        loadSession: false,
        mcpCapabilities: { http: true },
      },
      authMethods: [],
      agentInfo: {
        name: "troupe-pi-shim",
        title: "Troupe Pi bridge",
        version: BRIDGE_VERSION,
      },
    });
    return;
  }
  if (!acpInitialized) {
    if (id !== undefined) errorResponse(id, -32002, "Pi bridge is not initialized");
    return;
  }
  if (method === "session/new") {
    if (sessionId !== null) {
      errorResponse(id, -32002, "Pi session is already initialized");
      return;
    }
    sessionId = `pi-${Date.now()}-${Math.random().toString(16).slice(2)}`;
    routeServer = firstMcpServer(params.mcpServers);
    try {
      routeCredentials(routeServer);
    } catch {
      sessionId = null;
      routeServer = null;
      errorResponse(id, -32602, "invalid result route");
      return;
    }
    try {
      await startPi();
    } catch (error) {
      if (child && child.exitCode === null) child.kill();
      sessionId = null;
      routeServer = null;
      errorResponse(id, -32003, "Pi bridge could not start Pi", {
        reason: error instanceof Error ? error.message : "startup_failed",
      });
      return;
    }
    response(id, { sessionId, configOptions: configOptions() });
    return;
  }
  if (method === "session/set_config_option") {
    if (params.sessionId !== sessionId) {
      errorResponse(id, -32602, "invalid Pi session");
      return;
    }
    const configId = params.configId;
    const value = params.value;
    const expectedConfigIds = ["mode", "model"];
    if (requestedEffort !== null) expectedConfigIds.push("thinking");
    if (configId !== expectedConfigIds[configurationStep]) {
      errorResponse(id, -32602, "unsupported configuration option");
      return;
    }
    if (configId === "mode" && value !== "default") {
      errorResponse(id, -32602, "Pi mode is fixed for this session");
      return;
    }
    if (configId === "model" && value !== selectedModel) {
      errorResponse(id, -32602, "Pi model is fixed for this session");
      return;
    }
    if (configId === "thinking" && requestedEffort !== null && value !== requestedEffort) {
      errorResponse(id, -32602, "Pi thinking level is fixed for this session");
      return;
    }
    config[configId] = value;
    configurationStep += 1;
    response(id, { configOptions: configOptions() });
    return;
  }
  if (method === "session/prompt") {
    const expectedConfigCount = requestedEffort === null ? 2 : 3;
    if (params.sessionId !== sessionId || configurationStep !== expectedConfigCount || activePrompt) {
      errorResponse(id, -32603, "Pi session is busy");
      return;
    }
    const blocks = Array.isArray(params.prompt) ? params.prompt : [];
    if (
      blocks.length === 0 ||
      blocks.some((block) => block?.type !== "text" || typeof block.text !== "string")
    ) {
      errorResponse(id, -32602, "Pi bridge accepts text prompts only");
      return;
    }
    const text = blocks.map((block) => block.text).join("");
    const prompt = {
      id,
      cancelled: false,
      accepted: false,
      pendingStopReason: null,
    };
    activePrompt = prompt;
    lastProviderError = null;
    lastStopReason = "end_turn";
    try {
      await sendPi({ type: "prompt", message: text }, PI_RPC_PROMPT_ACCEPT_TIMEOUT_MS);
      if (activePrompt === prompt) {
        prompt.accepted = true;
        if (prompt.pendingStopReason !== null) settlePrompt(prompt.pendingStopReason);
        else armSettlementTimer(prompt);
      }
    } catch (error) {
      if (error instanceof Error && error.message === "Pi RPC command timed out") {
        failProtocol("prompt_timeout");
      } else if (error?.piErrorKind) {
        setProviderError(error.piErrorKind, error.piErrorReason);
        settlePrompt("end_turn", true);
      } else if (activePrompt?.cancelled) {
        activePrompt.cancelFailure = "prompt_abort_failed";
        failProtocol("prompt_abort_failed");
      } else {
        setProviderError("prompt", "prompt_rejected");
        settlePrompt("end_turn", true);
      }
    }
    return;
  }
  if (method === "session/cancel") {
    if (params.sessionId !== sessionId) {
      errorResponse(id, -32602, "invalid Pi session");
      return;
    }
    if (activePrompt) {
      activePrompt.cancelled = true;
      armCancelTimer();
      try {
        await sendPi({ type: "abort" }, PI_RPC_ABORT_TIMEOUT_MS);
      } catch {
        if (activePrompt) {
          // An abort acknowledgement is not a settlement boundary.  Kill the
          // bridge immediately when the provider cannot accept the abort;
          // otherwise a late result could cross into the next Act.
          activePrompt.cancelFailure = "abort_failed";
          failProtocol("abort_failed");
        }
      }
    }
    return;
  }
  if (id !== undefined) errorResponse(id, -32601, "Method not found");
}

let acpBuffer = "";
process.stdin.on("data", (chunk) => {
  acpBuffer += acpDecoder.write(chunk);
  while (true) {
    const newline = acpBuffer.indexOf("\n");
    if (newline < 0) break;
    let line = acpBuffer.slice(0, newline);
    acpBuffer = acpBuffer.slice(newline + 1);
    if (Buffer.byteLength(line, "utf8") > FRAME_MAX) {
      process.stderr.write("Troupe ACP frame is oversized\n");
      failProtocol("acp_frame_limit");
      return;
    }
    if (line.endsWith("\r")) line = line.slice(0, -1);
    if (!line) continue;
    let request;
    try {
      request = JSON.parse(line);
    } catch {
      errorResponse(null, -32700, "Parse error");
      continue;
    }
    if (request?.method) {
      void handleAcp(request).catch(() => {
        if (request.id !== undefined) errorResponse(request.id, -32603, "Pi bridge request failed");
      });
    }
  }
  if (Buffer.byteLength(acpBuffer, "utf8") > FRAME_MAX) {
    process.stderr.write("Troupe ACP frame is oversized\n");
    failProtocol("acp_frame_limit");
  }
});

process.stdin.on("end", () => {
  const tail = acpDecoder.end();
  if (tail.trim() !== "") failProtocol("protocol_error");
  shuttingDown = true;
  clearCancelTimer();
  clearSettlementTimer();
  if (child) child.kill();
  rejectPendingPi(new Error("ACP transport closed"));
});
