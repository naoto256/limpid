// Opt-in integration test: article fences are the source, not copied configs.
// LIMPID_BIN and LIMPIDCTL_BIN must point to the version being documented.
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import net from "node:net";
import http from "node:http";
import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { once } from "node:events";
import { spawnRecipe, withRecipeCleanup } from "./recipe-process.mjs";
const bin = process.env.LIMPID_BIN;
const ctl = process.env.LIMPIDCTL_BIN;
assert.ok(
  bin && ctl,
  "Set LIMPID_BIN and LIMPIDCTL_BIN; no production default",
);
assert.equal(
  execFileSync(bin, ["--version"], { encoding: "utf8" }).trim(),
  "limpid 0.9.0",
);
// macOS's per-user temporary directory can exceed the Unix socket path limit.
const root = fs.mkdtempSync("/tmp/limpid-article-live-");
fs.chmodSync(root, 0o700);
const repo = new URL("../../", import.meta.url);
const results = [];
const wait = () => new Promise((resolve) => setTimeout(resolve, 50));
async function until(check) {
  const deadline = Date.now() + 10000;
  while (!check()) {
    assert.ok(Date.now() < deadline, "bounded observation deadline");
    await wait();
  }
}
async function freePort() {
  const server = net.createServer();
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const port = server.address().port;
  await new Promise((resolve) => server.close(resolve));
  return port;
}
function article(name) {
  const bytes = fs.readFileSync(new URL(`../src/${name}.md`, import.meta.url));
  return {
    name,
    text: bytes.toString(),
    sha256: createHash("sha256").update(bytes).digest("hex"),
  };
}
function fences(text, language) {
  return [
    ...text.matchAll(new RegExp("```" + language + "\\n([\\s\\S]*?)```", "g")),
  ].map((m) => m[1].trimEnd());
}
function lines(file) {
  const text = fs.existsSync(file)
    ? fs.readFileSync(file, "utf8").trimEnd()
    : "";
  return text ? text.split("\n") : [];
}
async function daemon(config, dir) {
  fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
  fs.chmodSync(dir, 0o700);
  const filename = path.join(dir, "case.conf");
  fs.writeFileSync(filename, config, { mode: 0o600 });
  fs.writeFileSync(
    path.join(dir, "check.log"),
    execFileSync(bin, ["--check", "--config", filename], {
      encoding: "utf8",
      timeout: 5000,
    }),
  );
  const out = fs.openSync(path.join(dir, "stdout.log"), "w");
  let err, owned;
  try {
    try {
      err = fs.openSync(path.join(dir, "stderr.log"), "w");
      owned = spawnRecipe(bin, ["--config", filename], {
        stdio: ["ignore", out, err],
      });
    } finally {
      fs.closeSync(out);
      if (err !== undefined) fs.closeSync(err);
    }
    const { child, stop } = owned;
    const socket = path.join(dir, "control.sock");
    await until(() => {
      try {
        execFileSync(ctl, ["--socket", socket, "health"], {
          stdio: "pipe",
          timeout: 1000,
        });
        return true;
      } catch {
        assert.equal(child.exitCode, null, "daemon exited before health");
        return false;
      }
    });
    return { socket, stop, filename };
  } catch (e) {
    await withRecipeCleanup(async (defer) => {
      if (owned) defer(owned.stop);
      throw e;
    });
  }
}
function inject(instance, source, input, extraLines) {
  const command = source.text.match(
    /limpidctl --socket \S+ inject input ([\w_]+)( --json)? < (\S+)/,
  );
  assert.ok(command, `Missing published inject command in ${source.name}`);
  const args = [
    "--socket",
    instance.socket,
    "inject",
    "input",
    command[1],
    ...(command[2] ? ["--json"] : []),
  ];
  const data = extraLines ?? input;
  fs.writeFileSync(
    path.join(path.dirname(instance.filename), "events.jsonl"),
    data + "\n",
  );
  execFileSync(ctl, args, { input: data + "\n", timeout: 5000 });
}
function control(dir) {
  return `control {socket "${dir}/control.sock" error_log "${dir}/error.jsonl"}\n`;
}
async function jsonReceiver() {
  const requests = [];
  const server = http.createServer((req, res) => {
    let data = "";
    req.setEncoding("utf8");
    req.on("data", (chunk) => {
      data += chunk;
    });
    req.on("end", () => {
      requests.push({ body: JSON.parse(data), headers: req.headers });
      res.writeHead(200);
      res.end("{}");
    });
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  return {
    requests,
    port: server.address().port,
    close: () => new Promise((resolve) => server.close(resolve)),
  };
}
const nr = article("new-relic");
const nrBlocks = fences(nr.text, "limpid");
assert.equal(nrBlocks.length, 4);
const eventText = fences(nr.text, "json")[0];
assert.equal(
  eventText.split("\n").length,
  1,
  "Live Event injection is JSONL, not pretty-printed JSON",
);
const event = JSON.parse(eventText);
assert.ok(Number.isInteger(event.received_at));
for (const [name, baseIndex, parsed] of [
  ["json-raw", 0, false],
  ["json-parsed", 0, true],
  ["otlp-raw", 2, false],
  ["otlp-parsed", 2, true],
  ["otlp-parsed-wire-body", 2, true],
]) {
  const dir = path.join(root, name);
  fs.mkdirSync(dir);
  fs.cpSync(
    new URL("packaging/snippets", repo),
    path.join(dir, "packaging/snippets"),
    { recursive: true },
  );
  const isOtlp = name.startsWith("otlp");
  let receiver, sink;
  const received = path.join(dir, "receiver", "received.jsonl");
  await withRecipeCleanup(async (defer) => {
    const port = await freePort();
    if (isOtlp) {
      const recvDir = path.join(dir, "receiver");
      receiver = await daemon(
        control(recvDir) +
          `def input wire {type otlp_http bind "127.0.0.1:${port}"}\ndef output records {type file path "${received}"}\ndef process decode {egress = to_json(otlp.decode_resourcelog_protobuf(ingress))}\ndef pipeline capture {input wire process decode output records}`,
        recvDir,
      );
      defer(receiver.stop);
    } else {
      sink = await jsonReceiver();
      defer(() => sink.close());
    }
    let conf = nrBlocks[baseIndex];
    if (parsed)
      conf =
        conf.slice(0, conf.indexOf("def pipeline")) + nrBlocks[baseIndex + 1];
    const wireBody = name === "otlp-parsed-wire-body";
    if (wireBody) {
      const assignment = nr.text.match(
        /assigning `([^`]+)` after the adapter/,
      )[1];
      conf =
        conf.replace("| compose_otlp", "| retain_wire | compose_otlp") +
        `\ndef process retain_wire {${assignment}}\n`;
    }
    const substitutions = [
      ["127.0.0.1:5514", `127.0.0.1:${await freePort()}`],
      ["<NEW_RELIC_LICENSE_KEY>", "SYNTHETIC_LOCAL_KEY"],
      [
        isOtlp
          ? "https://otlp.nr-data.net/v1/logs"
          : "https://log-api.newrelic.com/log/v1",
        `http://127.0.0.1:${isOtlp ? port : sink.port}${isOtlp ? "/v1/logs" : "/"}`,
      ],
    ];
    for (const [from, to] of substitutions) {
      assert.ok(conf.includes(from));
      conf = conf.replaceAll(from, to);
    }
    fs.writeFileSync(
      path.join(dir, "substitutions.json"),
      JSON.stringify(substitutions),
    );
    let sender;
    sender = await daemon(control(dir) + conf, dir);
    defer(sender.stop);
    // Execute the published pipeline-inspection form too; OTLP decode is inspection-only.
    let inspect = control(dir) + conf;
    if (isOtlp)
      inspect =
        inspect.replace("| otlp_to_egress", "| otlp_to_egress | inspect") +
        "\ndef process inspect {egress = to_json(otlp.decode_resourcelog_protobuf(egress))}\n";
    fs.writeFileSync(path.join(dir, "inspect.conf"), inspect);
    const trace = execFileSync(
      bin,
      [
        "--test-pipeline",
        "syslog_to_newrelic",
        "--config",
        path.join(dir, "inspect.conf"),
        "--input",
        JSON.stringify(event),
      ],
      { encoding: "utf8", timeout: 5000 },
    );
    fs.writeFileSync(path.join(dir, "pipeline.log"), trace);
    assert.match(trace, /egress:/);
    inject(sender, nr, eventText);
    await until(() =>
      isOtlp ? lines(received).length === 1 : sink.requests.length === 1,
    );
    await sender.stop();
    sender = null;
    if (receiver) {
      await receiver.stop();
      receiver = null;
    }
    let actual;
    if (isOtlp) {
      actual = JSON.parse(lines(received)[0]);
      const log = actual.scope_logs[0].log_records[0];
      const attrs = Object.fromEntries(
        log.attributes.map((a) => [
          a.key,
          a.value.string_value ?? a.value.int_value,
        ]),
      );
      if (parsed) {
        assert.equal(log.severity_number, 19);
        assert.equal(attrs["source.ip"], "192.0.2.10");
        assert.equal(attrs["destination.port"], 9100);
        assert.equal(attrs["rule.name"], "Example.Signature");
        assert.equal(
          log.body.string_value,
          wireBody
            ? event.ingress
            : event.ingress.slice(event.ingress.indexOf("CEF:")),
        );
      } else {
        assert.equal(log.body.string_value, event.ingress);
        assert.equal(attrs.route, "otlp");
        assert.equal(log.time_unix_nano, event.received_at);
        assert.ok(
          actual.resource.attributes.some(
            (a) =>
              a.key === "service.name" &&
              a.value.string_value === "syslog-forwarder",
          ),
        );
      }
    } else {
      assert.equal(sink.requests[0].headers["api-key"], "SYNTHETIC_LOCAL_KEY");
      assert.equal(
        sink.requests[0].headers["content-type"],
        "application/json",
      );
      actual = sink.requests[0].body;
      assert.equal(actual.message, event.ingress);
      if (parsed) {
        assert.equal(actual.source.ip, "192.0.2.10");
        assert.equal(actual.destination.port, 9100);
        assert.equal(actual.rule.name, "Example.Signature");
        assert.equal(actual.severity_number, 19);
      } else
        assert.deepEqual(actual, {
          message: event.ingress,
          service: "syslog-forwarder",
          route: "json",
        });
    }
    assert.equal(lines(path.join(dir, "error.jsonl")).length, 0);
    assert.ok(
      !fs
        .readFileSync(path.join(dir, "stdout.log"), "utf8")
        .includes("skipping invalid JSON"),
    );
    fs.writeFileSync(
      path.join(dir, "received.json"),
      JSON.stringify(actual, null, 2),
    );
    results.push({
      article: nr.name,
      sha256: nr.sha256,
      case: name,
      pass: true,
      injected: 1,
      received: 1,
    });
    console.log(`${name}: PASS`);
  });
}
for (const name of [
  "safe-forwarding",
  "quarantine-invalid",
  "asset-enrichment",
]) {
  const source = article(name);
  const dir = path.join(root, name);
  const safe = name === "safe-forwarding";
  const quarantine = name === "quarantine-invalid";
  let fixture = fences(source.text, quarantine ? "text" : "json")[0];
  if (safe) {
    assert.equal(
      fixture.split("\n").length,
      1,
      "Published JSONL must be one physical line",
    );
    const base = JSON.parse(fixture);
    fixture = [
      fixture,
      JSON.stringify({
        ...base,
        event_id: "evt-0002",
        action: "logout",
        outcome: "failure",
        extra: { new: { copy: "FAKE_SECOND_ONLY" } },
      }),
      "{bad-json FAKE_BAD_ONLY",
      JSON.stringify({ ...base, event_id: undefined }),
      JSON.stringify({ ...base, event_id: 123 }),
      JSON.stringify({ ...base, action: "FAKE_ACTION_ONLY" }),
      JSON.stringify({ ...base, outcome: { secret: "FAKE_OUTCOME_ONLY" } }),
      JSON.stringify({ ...base, event_id: "FAKE_ID_ONLY" }),
    ].join("\n");
  }
  await withRecipeCleanup(async (defer) => {
    const sink = safe ? await jsonReceiver() : null;
    if (sink) defer(() => sink.close());
    let config = fences(source.text, "limpid")[0]
      .replaceAll(
        safe
          ? "/tmp/limpid-safe"
          : quarantine
            ? "/tmp/limpid-quarantine"
            : "/tmp/limpid-assets",
        dir,
      )
      .replace("127.0.0.1:15514", `127.0.0.1:${await freePort()}`);
    if (safe)
      config = config.replace("127.0.0.1:18080", `127.0.0.1:${sink.port}`);
    let instance;
    const read = (file) => lines(path.join(dir, file));
    instance = await daemon(config, dir);
    defer(instance.stop);
    inject(instance, source, fixture);
    await until(() =>
      safe
        ? sink.requests.length === 2 &&
          read("original.jsonl").length === 8 &&
          read("error.jsonl").length === 6
        : quarantine
          ? read("original.jsonl").length === 4 &&
            read("accepted.jsonl").length === 2 &&
            read("malformed.jsonl").length === 1 &&
            read("error.jsonl").length === 1
          : read("enriched.jsonl").length === 3,
    );
    await instance.stop();
    instance = null;
    if (safe || quarantine)
      assert.deepEqual(
        read("original.jsonl").sort(),
        fixture.split("\n").sort(),
      );
    if (safe) {
      assert.deepEqual(
        sink.requests
          .map((x) => x.body)
          .sort((a, b) => a.event_id.localeCompare(b.event_id)),
        [
          JSON.parse(fences(source.text, "json")[1]),
          { event_id: "evt-0002", action: "logout", outcome: "failure" },
        ],
      );
      assert.equal(read("error.jsonl").length, 6);
    } else if (quarantine) {
      assert.deepEqual(
        read("accepted.jsonl")
          .map(JSON.parse)
          .sort((a, b) => a.action.localeCompare(b.action)),
        [{ action: "login" }, { action: "logout" }],
      );
      assert.deepEqual(JSON.parse(read("malformed.jsonl")[0]), {
        reason: "invalid JSON",
        message: "{broken-json",
      });
      assert.match(read("error.jsonl")[0], /unsupported application action/);
      assert.match(read("error.jsonl")[0], /new_action/);
    } else {
      const rows = read("enriched.jsonl")
        .map(JSON.parse)
        .sort((a, b) => a.sender_ip.localeCompare(b.sender_ip));
      const inputs = fixture.split("\n").map(JSON.parse);
      const expected = [
        {
          known: true,
          name: "edge-fw-a",
          site: "tokyo",
          environment: "production",
        },
        { known: true, name: "lab-fw-b", site: "osaka", environment: "test" },
        { known: false },
      ];
      rows.forEach((row, i) =>
        assert.deepEqual(row, {
          message: inputs[i].ingress,
          sender_ip: inputs[i].source.ip,
          asset: expected[i],
        }),
      );
      assert.equal(read("error.jsonl").length, 0);
    }
    results.push({
      article: name,
      sha256: source.sha256,
      pass: true,
      injected: fixture.split("\n").length,
      normal_exit: 0,
    });
    console.log(`${name}: PASS`);
  });
}
fs.writeFileSync(
  path.join(root, "results.json"),
  JSON.stringify(results, null, 2),
);
console.log(`Evidence: ${root}`);
