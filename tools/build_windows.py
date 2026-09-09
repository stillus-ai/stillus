#!/usr/bin/env python3
# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only
"""Cross-build and inspect portable Windows artifacts inside the toolchain."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parent.parent
TARGET = "x86_64-pc-windows-gnu"
OUTPUT = ROOT / "dist/windows/x86_64"
SYSTEM_DLLS = set("""advapi32 avrt bcrypt bcryptprimitives cfgmgr32 comctl32 comdlg32
crypt32 d2d1 d3d11 d3d12 d3dcompiler_47 dcomp dnsapi dwmapi dwrite dxgi gdi32
hid imm32 iphlpapi kernel32 msvcrt ncrypt ntdll ole32 oleaut32 opengl32 powrprof
propsys psapi rpcrt4 secur32 setupapi shell32 shcore shlwapi ucrtbase user32
userenv usp10 uxtheme version windowscodecs winhttp winmm winspool wintrust
ws2_32 wsock32 wtsapi32 normaliz netapi32 dbghelp runtimeobject""".split())


def run(*command: str, capture: bool = False) -> str:
    result = subprocess.run(command, cwd=ROOT, check=False, text=True,
                            stdout=subprocess.PIPE if capture else None)
    if result.returncode and result.stdout:
        for line in result.stdout.splitlines():
            try:
                record = json.loads(line)
            except json.JSONDecodeError:
                print(line, file=sys.stderr)
                continue
            if record.get("reason") == "compiler-message":
                print(record["message"].get("rendered", record["message"]["message"]), file=sys.stderr)
    result.check_returncode()
    return result.stdout or ""


def inspect_pe(path: Path, *, gui: bool = False) -> list[str]:
    with path.open("rb") as source:
        if source.read(2) != b"MZ":
            raise ValueError(f"not a PE file: {path}")
        source.seek(0x3C)
        offset = struct.unpack("<I", source.read(4))[0]
        source.seek(offset)
        if source.read(4) != b"PE\0\0" or struct.unpack("<H", source.read(2))[0] != 0x8664:
            raise ValueError(f"not x86_64 PE: {path}")
        source.seek(offset + 24)
        if struct.unpack("<H", source.read(2))[0] != 0x20B:
            raise ValueError(f"not PE32+: {path}")
        source.seek(offset + 24 + 68)
        subsystem = struct.unpack("<H", source.read(2))[0]
        if gui and subsystem != 2:
            raise ValueError(f"application would open a console: {path}")
        if gui:
            source.seek(offset + 24 + 112 + 2 * 8)
            address, size = struct.unpack("<II", source.read(8))
            if not address or not size:
                raise ValueError(f"application resources are missing: {path}")
    imports = run("x86_64-w64-mingw32-objdump", "-p", str(path), capture=True)
    return re.findall(r"DLL Name:\s*(\S+)", imports)


def bundle_dependencies(binaries: list[Path], destination: Path) -> dict[str, list[str]]:
    queue = list(binaries)
    checked = {}
    while queue:
        binary = queue.pop()
        if binary.name.lower() in checked:
            continue
        dependencies = inspect_pe(binary)
        checked[binary.name.lower()] = dependencies
        for name in dependencies:
            lowered = name.lower()
            if Path(lowered).stem in SYSTEM_DLLS or lowered.startswith(("api-ms-win-", "ext-ms-win-")):
                continue
            runtime = destination / name
            if not runtime.exists():
                resolved = run("x86_64-w64-mingw32-gcc", f"-print-file-name={name}", capture=True).strip()
                source = Path(resolved)
                if not source.is_file():
                    candidates = list(Path("/usr/lib/gcc/x86_64-w64-mingw32").rglob(name))
                    if not candidates:
                        raise ValueError(f"unresolved non-system DLL {name} required by {binary.name}")
                    source = candidates[0]
                shutil.copy2(source, runtime)
            queue.append(runtime)
    return checked


def resource_object(directory: Path) -> Path:
    icon = directory / "Stillus.ico"
    run("convert", "-background", "none", str(ROOT / "app/stillus/assets/stillus-app-icon.svg"),
        "-define", "icon:auto-resize=256,128,64,48,32,16", str(icon))
    version = tomllib.loads((ROOT / "app/stillus/Cargo.toml").read_text())["package"]["version"]
    numeric = ",".join([*version.split("."), "0"])
    manifest = directory / "Stillus.manifest"
    manifest.write_text('''<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
 <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3"><security><requestedPrivileges>
  <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
 </requestedPrivileges></security></trustInfo>
 <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1"><application>
  <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
 </application></compatibility>
 <application xmlns="urn:schemas-microsoft-com:asm.v3"><windowsSettings>
  <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
  <longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware>
 </windowsSettings></application>
</assembly>
''', encoding="utf-8")
    script = directory / "Stillus.rc"
    script.write_text(f'''1 ICON "{icon}"
1 24 "{manifest}"
1 VERSIONINFO
FILEVERSION {numeric}
PRODUCTVERSION {numeric}
FILEOS 0x40004
FILETYPE 1
BEGIN
 BLOCK "StringFileInfo"
 BEGIN
  BLOCK "040904B0"
  BEGIN
   VALUE "CompanyName", "Evgeniy Udodov\\0"
   VALUE "FileDescription", "Stillus\\0"
   VALUE "FileVersion", "{version}\\0"
   VALUE "InternalName", "Stillus\\0"
   VALUE "OriginalFilename", "Stillus.exe\\0"
   VALUE "ProductName", "Stillus\\0"
   VALUE "ProductVersion", "{version}\\0"
   VALUE "LegalCopyright", "Copyright 2026 Evgeniy Udodov. GPL-3.0-only.\\0"
  END
 END
 BLOCK "VarFileInfo"
 BEGIN
  VALUE "Translation", 0x409, 1200
 END
END
''', encoding="utf-8")
    fingerprint = hashlib.sha256(script.read_bytes() + icon.read_bytes() + manifest.read_bytes()).hexdigest()[:16]
    obj = directory / f"Stillus-{fingerprint}.o"
    run("x86_64-w64-mingw32-windres", "-i", str(script), "-o", str(obj), "-O", "coff")
    return obj


def previous_runtime_names(destination: Path) -> set[str]:
    manifest = destination / "dependencies.json"
    if not manifest.exists():
        return set()
    records = json.loads(manifest.read_text())
    return {name.lower() for name in records
            if Path(name).name == name and "\\" not in name and name.lower().endswith(".dll")}


def package_application() -> None:
    target_directory = Path(os.environ["CARGO_TARGET_DIR"])
    resources = target_directory / "stillus-windows-resources"
    resources.mkdir(exist_ok=True)
    obj = resource_object(resources)
    run("cargo", "rustc", "--locked", "--release", "-p", "stillus-app", "--bin", "stillus-app",
        "--target", TARGET, "--", f"-Clink-arg={obj}")
    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".package-", dir=OUTPUT.parent) as temporary:
        staged = Path(temporary)
        executable = staged / "Stillus.exe"
        shutil.copy2(target_directory / TARGET / "release/stillus-app.exe", executable)
        run("x86_64-w64-mingw32-strip", str(executable))
        inspect_pe(executable, gui=True)
        dependencies = bundle_dependencies([executable], staged)
        shutil.copy2(ROOT / "LICENSE", staged / "LICENSE.txt")
        shutil.copy2(ROOT / "tools/register_windows.ps1", staged / "Register.ps1")
        if list(staged.glob("*.dll")):
            shutil.copy2("/usr/share/doc/mingw-w64-common/copyright", staged / "MinGW-LICENSE.txt")
        (staged / "dependencies.json").write_text(json.dumps(dependencies, indent=2) + "\n")
        OUTPUT.mkdir(exist_ok=True)
        # Only replace Windows package files, retaining the separately built test kit.
        previous = previous_runtime_names(OUTPUT) | {
            "stillus.exe", "license.txt", "mingw-license.txt", "dependencies.json", "register.ps1"
        }
        for old in OUTPUT.iterdir():
            if old.is_file() and old.name.lower() in previous:
                old.unlink()
        for artifact in staged.iterdir():
            shutil.move(artifact, OUTPUT / artifact.name)
    print(f"BUILT_WINDOWS_APP path={OUTPUT / 'Stillus.exe'} architecture=x86_64")


def package_tests() -> None:
    destination = OUTPUT / "tests"
    records = run("cargo", "test", "--locked", "--workspace", "--all-features", "--no-run",
                  "--target", TARGET, "--message-format=json", capture=True)
    tests = []
    for line in records.splitlines():
        record = json.loads(line)
        if record.get("reason") == "compiler-artifact" and record.get("profile", {}).get("test") and record.get("executable"):
            tests.append(Path(record["executable"]))
    if not tests:
        raise ValueError("Cargo returned no Windows test executables")
    destination.mkdir(parents=True, exist_ok=True)
    previous = previous_runtime_names(destination) | {"tests.json", "dependencies.json", "run-tests.ps1"}
    test_manifest = destination / "tests.json"
    if test_manifest.exists():
        previous.update(name.lower() for name in json.loads(test_manifest.read_text())
                        if Path(name).name == name and "\\" not in name and name.lower().endswith(".exe"))
    for old in destination.iterdir():
        if old.is_file() and old.name.lower() in previous:
            old.unlink()
    for executable in tests:
        shutil.copy2(executable, destination / executable.name)
    dependencies = bundle_dependencies([destination / path.name for path in tests], destination)
    (destination / "tests.json").write_text(json.dumps(sorted(path.name for path in tests), indent=2) + "\n")
    (destination / "dependencies.json").write_text(json.dumps(dependencies, indent=2) + "\n")
    shutil.copy2(ROOT / "tools/test_windows.ps1", destination / "Run-Tests.ps1")
    for name in ("ci_diagnostics.py", "windows_test_support.ps1", "test_windows_support.ps1"):
        shutil.copy2(ROOT / "tools" / name, destination / name)
    print(f"BUILT_WINDOWS_TESTS path={destination} executables={len(tests)}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tests", action="store_true")
    args = parser.parse_args()
    if args.tests:
        package_tests()
    else:
        package_application()


if __name__ == "__main__":
    main()
