"""Smoke-test the native executable, inventory/schema and stdio handshake."""
import argparse
import json
import os
from pathlib import Path
import queue
import subprocess
import threading
import tomllib

ROOT = Path(__file__).resolve().parents[1]


def require(condition, message):
    if not condition:
        raise SystemExit(message)


parser = argparse.ArgumentParser()
parser.add_argument("binary", type=Path)
args = parser.parse_args()
binary = str(args.binary.resolve(strict=True))
version = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["package"]["version"]
expected = json.loads((ROOT / "spec/tool_schema_snapshot.v1.json").read_text(encoding="utf-8"))
env = dict(os.environ, GOOGLE_ANALYTICS_MCP_CAPABILITY_PROFILE="read_only")


def command(flag, selected_env=env):
    return subprocess.check_output(
        [binary, flag], text=True, env=selected_env, timeout=60
    ).strip()


require(command("--version") == f"ga4-mcp {version}", "Unexpected binary version")
for profile in ("read_only", "scratchpad"):
    profile_env = dict(env, GOOGLE_ANALYTICS_MCP_CAPABILITY_PROFILE=profile)
    actual_tools = json.loads(command("--print-tools", profile_env))
    require(sorted(actual_tools) == sorted(tool["name"] for tool in expected["tools"]),
            f"Tool inventory differs from snapshot ({profile})")
    require(json.loads(command("--print-tool-schema", profile_env)) == expected,
            f"Tool schema differs from snapshot ({profile})")

proc = subprocess.Popen(
    [binary], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
    stderr=subprocess.DEVNULL, text=True, encoding="utf-8", env=env,
)
responses = queue.Queue()


def read_lines():
    for line in proc.stdout:
        responses.put(line)
    responses.put(None)


threading.Thread(target=read_lines, daemon=True).start()


def send(message):
    proc.stdin.write(json.dumps({"jsonrpc": "2.0", **message}) + "\n")
    proc.stdin.flush()


def receive(request_id):
    for _ in range(20):
        try:
            line = responses.get(timeout=15)
        except queue.Empty:
            raise SystemExit("MCP response timed out") from None
        require(line is not None, "MCP process exited before response")
        message = json.loads(line)
        if message.get("id") == request_id:
            require("error" not in message, "MCP returned a protocol error")
            return message["result"]
    raise SystemExit("Expected MCP response not received")


try:
    send({
        "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "ga4-mcp-release-smoke", "version": "1.0.0"}
        }
    })
    initialized = receive(1)
    require(bool(initialized.get("serverInfo")), "Missing MCP server identity")
    send({"method": "notifications/initialized"})
    send({"id": 2, "method": "tools/list", "params": {}})
    tools = receive(2)["tools"]
    require({"ga4_get_started", "run_report", "ga4_auth_status"} <=
            {tool["name"] for tool in tools}, "Required tools not exposed")
finally:
    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5)

print("Release binary version, tool snapshot and stdio handshake passed")
