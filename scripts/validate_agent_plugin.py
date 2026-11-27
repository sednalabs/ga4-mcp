"""Validate official Agent Plugins schemas and GA4 package invariants."""
import json
import re
import tomllib
import urllib.request
from pathlib import Path

from jsonschema import Draft202012Validator

ROOT = Path(__file__).resolve().parents[1]
SCHEMA_BASE = "https://agent-plugins.org/schemas/1.0.0/"


def require(condition, message):
    if not condition:
        raise SystemExit(message)


def validate_manifest(filename):
    manifest = json.loads((ROOT / filename).read_text(encoding="utf-8"))
    expected = SCHEMA_BASE + filename.replace(".json", ".schema.json")
    require(manifest.get("$schema") == expected, f"Wrong schema: {filename}")
    with urllib.request.urlopen(expected, timeout=30) as response:
        require(response.geturl() == expected, "Unexpected schema redirect")
        schema = json.load(response)
    require(schema.get("$id") == expected, f"Wrong schema ID: {filename}")
    Draft202012Validator.check_schema(schema)
    Draft202012Validator(schema).validate(manifest)
    return manifest


plugin = validate_manifest("plugin.json")
mcp = validate_manifest("mcp.json")
package = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["package"]
require(plugin["name"] == package["name"] == "ga4-mcp", "Plugin name mismatch")
require(plugin["version"] == package["version"], "Plugin version mismatch")
require(bool(plugin["description"]) and bool(plugin["author"]["name"]), "Missing metadata")
require(set(plugin["keywords"]) == {
    "ga4", "google analytics", "analytics", "traffic", "sessions",
    "conversions", "funnel", "realtime"
}, "Unexpected plugin keywords")
require(mcp["mcpServers"] == {"ga4": {
    "type": "stdio",
    "command": "ga4-mcp",
    "env": {
        "GOOGLE_ANALYTICS_MCP_CAPABILITY_PROFILE": "read_only",
        "GOOGLE_ANALYTICS_MCP_QUOTA_PROJECT": "$" + "{GOOGLE_ANALYTICS_MCP_QUOTA_PROJECT}"
    },
}}, "MCP manifest must remain read-only and secret-free")

skill = ROOT / "skills" / "ga4-mcp" / "SKILL.md"
match = re.match(r"^---\n(.*?)\n---\n", skill.read_text(encoding="utf-8"), re.S)
require(match is not None, "Missing skill frontmatter")
frontmatter = dict(line.split(": ", 1) for line in match[1].splitlines())
require(frontmatter["name"] == skill.parent.name == plugin["name"], "Skill name mismatch")
require(0 < len(frontmatter["description"]) < 1024, "Invalid skill description")
require(not (ROOT / "POWER.md").exists(), "Legacy Power manifest")
require(not (ROOT / ".kiro" / "skills" / "ga4-mcp" / "SKILL.md").exists(),
        "Duplicate skill: retain only the canonical package copy")
print("Agent Plugins 1.0.0 schemas and GA4 Power invariants passed")
