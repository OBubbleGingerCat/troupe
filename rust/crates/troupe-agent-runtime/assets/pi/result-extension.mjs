import http from "node:http";
import https from "node:https";

const endpoint = process.env.TROUPE_RESULT_ENDPOINT;
const authorization = process.env.TROUPE_RESULT_AUTHORIZATION;
const revision = process.env.TROUPE_RESULT_REVISION || "2025-11-25";
const origin = process.env.TROUPE_RESULT_ORIGIN;
const RESULT_TOOL = "troupe_submit_result";
const REQUEST_TIMEOUT_MS = 30_000;
const REQUEST_MAX_BYTES = 8 * 1024 * 1024;
const RESPONSE_MAX_BYTES = 8 * 1024 * 1024;

if (!endpoint || !authorization || !origin) {
  throw new Error("Troupe result route is not configured");
}
if (
  /[\r\n]/.test(authorization) ||
  authorization.length > 4 * 1024 ||
  !authorization.startsWith("Bearer ") ||
  authorization.length <= "Bearer ".length
) {
  throw new Error("Troupe result route authorization is invalid");
}

const url = new URL(endpoint);
if (url.protocol !== "http:" && url.protocol !== "https:") {
  throw new Error("Troupe result route must use HTTP or HTTPS");
}
if (
  url.hostname !== "127.0.0.1" ||
  url.pathname !== "/mcp" ||
  url.username !== "" ||
  url.password !== "" ||
  url.search !== "" ||
  url.hash !== ""
) {
  throw new Error("Troupe result route must be the loopback MCP endpoint");
}
let originUrl;
try {
  originUrl = new URL(origin);
} catch {
  throw new Error("Troupe result route origin is invalid");
}
if (originUrl.origin !== url.origin) {
  throw new Error("Troupe result route origin does not match endpoint");
}
const transport = url.protocol === "https:" ? https : http;
const agent = new transport.Agent({ keepAlive: true, maxSockets: 1 });
let nextId = 1;
let lifecycle = null;
let lifecyclePromise = null;
let queue = Promise.resolve();

function request(body, initialized) {
  let payload;
  try {
    payload = JSON.stringify(body);
  } catch {
    return Promise.reject(new Error("result route request is not JSON-serializable"));
  }
  if (Buffer.byteLength(payload, "utf8") > REQUEST_MAX_BYTES) {
    return Promise.reject(new Error("result route request is oversized"));
  }
  const headers = {
    Accept: "application/json, text/event-stream",
    Authorization: authorization,
    "Content-Type": "application/json",
    "Content-Length": Buffer.byteLength(payload),
    Connection: "keep-alive",
    Origin: origin,
  };
  if (initialized) headers["MCP-Protocol-Version"] = revision;
  return new Promise((resolve, reject) => {
    let settled = false;
    let responseBytes = 0;
    let timer = null;
    const fail = (error) => {
      if (settled) return;
      settled = true;
      if (timer !== null) clearTimeout(timer);
      reject(error);
    };
    const succeed = (value) => {
      if (settled) return;
      settled = true;
      if (timer !== null) clearTimeout(timer);
      resolve(value);
    };
    const req = transport.request(
      {
        protocol: url.protocol,
        hostname: url.hostname,
        port: url.port || undefined,
        path: `${url.pathname}${url.search}`,
        method: "POST",
        headers,
        agent,
      },
      (res) => {
        const chunks = [];
        res.on("data", (chunk) => {
          if (settled) return;
          responseBytes += chunk.length;
          if (responseBytes > RESPONSE_MAX_BYTES) {
            res.destroy();
            req.destroy();
            fail(new Error("result route response is oversized"));
            return;
          }
          chunks.push(chunk);
        });
        res.on("end", () => {
          if (settled) return;
          const text = Buffer.concat(chunks).toString("utf8");
          if (res.statusCode === 202 && text.length === 0) {
            succeed({ status: res.statusCode, body: null });
            return;
          }
          let parsed;
          try {
            parsed = JSON.parse(text);
          } catch {
            fail(new Error("result route returned malformed JSON"));
            return;
          }
          if ((res.statusCode || 500) >= 400) {
            fail(new Error("result route rejected the request"));
            return;
          }
          succeed({ status: res.statusCode, body: parsed });
        });
        res.on("aborted", () => fail(new Error("result route response was aborted")));
        res.on("error", () => fail(new Error("result route response failed")));
      },
    );
    timer = setTimeout(() => {
      req.destroy();
      fail(new Error("result route request timed out"));
    }, REQUEST_TIMEOUT_MS);
    req.on("error", () => fail(new Error("result route request failed")));
    req.end(payload);
  });
}

function serialized(body, initialized) {
  const task = queue.then(() => request(body, initialized));
  queue = task.catch(() => undefined);
  return task;
}

async function ensureLifecycle() {
  if (lifecycle) return;
  if (!lifecyclePromise) {
    lifecyclePromise = (async () => {
      const initialized = await serialized(
        {
          jsonrpc: "2.0",
          id: nextId++,
          method: "initialize",
          params: {
            protocolVersion: revision,
            capabilities: {},
            clientInfo: { name: "troupe-pi-result-extension", version: "0.1.0" },
          },
        },
        false,
      );
      if (!initialized.body?.result || initialized.body.error) {
        throw new Error("result route initialize failed");
      }
      await serialized({ jsonrpc: "2.0", method: "notifications/initialized", params: {} }, true);
      const tools = await serialized(
        { jsonrpc: "2.0", id: nextId++, method: "tools/list", params: {} },
        true,
      );
      if (tools.body?.error) throw new Error("result route tools discovery failed");
      const names = tools.body?.result?.tools?.map((tool) => tool?.name) || [];
      if (!names.includes(RESULT_TOOL)) throw new Error("result tool is unavailable");
      lifecycle = true;
    })().catch((error) => {
      lifecyclePromise = null;
      throw error;
    });
  }
  await lifecyclePromise;
}

async function submit(value) {
  await ensureLifecycle();
  const result = await serialized(
    {
      jsonrpc: "2.0",
      id: nextId++,
      method: "tools/call",
      params: { name: RESULT_TOOL, arguments: { value } },
    },
    true,
  );
  const toolResult = result.body?.result;
  if (!toolResult || typeof toolResult.isError !== "boolean") {
    throw new Error("result route returned an invalid tool response");
  }
  if (toolResult.isError) {
    const text = Array.isArray(toolResult.content)
      ? toolResult.content.find((item) => item?.type === "text")?.text
      : undefined;
    // Pi marks a tool result as isError only when execute() throws. Returning
    // a value here would look like a successful tool call and could cause the
    // model to stop repairing an invalid schema value. The route already
    // bounds and sanitizes its validation detail; keep one more local bound
    // before handing that actionable feedback back to Pi.
    const feedback = typeof text === "string" ? text.slice(0, 4 * 1024) : "result rejected";
    throw new Error(feedback);
  }
  return {
    content: [{ type: "text", text: "Result accepted by Troupe." }],
    details: { accepted: true },
    terminate: true,
  };
}

export default function register(pi) {
  // Troupe waits for the result route during ACP opening. Establish the MCP
  // lifecycle now so prompt delivery does not depend on the first tool call.
  void ensureLifecycle().catch(() => {});
  pi.registerTool({
    name: RESULT_TOOL,
    label: "Submit result",
    description:
      "Submit the structured result for this Troupe act. Put the complete JSON object in the value field; retry with the validation feedback if it is rejected.",
    promptSnippet: "Submit the validated structured result to Troupe",
    promptGuidelines: [
      "Use troupe_submit_result exactly once when the structured result is ready.",
      "Put the complete result object under the value field; do not submit prose or markdown.",
      "If troupe_submit_result returns validation feedback, repair the value and call it again.",
    ],
    parameters: {
      type: "object",
      properties: { value: { type: "object", additionalProperties: true } },
      required: ["value"],
      additionalProperties: false,
    },
    async execute(_toolCallId, params) {
      if (!params || typeof params.value !== "object" || params.value === null || Array.isArray(params.value)) {
        // Throw so Pi marks this execution as `isError` and gives the model
        // a chance to repair the malformed tool arguments.
        throw new Error("value must be a JSON object");
      }
      return submit(params.value);
    },
  });
}
