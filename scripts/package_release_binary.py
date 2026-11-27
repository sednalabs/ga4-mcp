"""Produce the native ZIP and its SHA-256 sidecar."""
import argparse
import hashlib
from pathlib import Path
import platform
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument("platform", choices=(
    "linux-x86_64", "windows-x86_64", "macos-x86_64", "macos-aarch64",
))
parser.add_argument("binary", type=Path)
args = parser.parse_args()
native_os = {"Linux": "linux", "Windows": "windows", "Darwin": "macos"}[platform.system()]
arch = {"amd64": "x86_64", "x86_64": "x86_64", "arm64": "aarch64",
        "aarch64": "aarch64"}[platform.machine().lower()]
if args.platform != f"{native_os}-{arch}":
    raise SystemExit("Release runner does not match requested platform")
version = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["package"]["version"]
dist = ROOT / "dist"
dist.mkdir(exist_ok=True)
archive = dist / f"ga4-mcp-v{version}-{args.platform}.zip"
binary_name = "ga4-mcp.exe" if native_os == "windows" else "ga4-mcp"
with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as bundle:
    bundle.write(args.binary.resolve(strict=True), binary_name)
with archive.open("rb") as handle:
    digest = hashlib.file_digest(handle, "sha256").hexdigest()
archive.with_suffix(".zip.sha256").write_text(
    f"{digest}  {archive.name}\n", encoding="ascii", newline="\n"
)
print(archive.name)
