#!/usr/bin/env python3
# Copyright 2026 Evgeniy Udodov
# SPDX-License-Identifier: GPL-3.0-only
"""CI orchestration and allowlisted artifacts; behavioral checks stay in Make/test scripts."""

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import platform
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
from datetime import datetime, timezone
import zipfile

from source_revision import validate_revision
from ci_diagnostics import safe_line

ROOT = Path(__file__).resolve().parent.parent
REPORTS = ROOT / ".ci/reports"
ARTIFACTS = ROOT / ".ci/artifacts"


def revision():
    value = os.environ.get("SOURCE_REVISION", "")
    return validate_revision(value, value)


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def initialize(name):
    write_json(REPORTS / name / "status.json", {
        "platform": name, "architecture": "arm64" if name == "macos" else platform.machine(),
        "controller_architecture": platform.machine(),
        "source_revision": revision(), "status": "incomplete",
        "started": datetime.now(timezone.utc).isoformat(),
    })


def finish(name, status):
    path = REPORTS / name / "status.json"
    report = json.loads(path.read_text()) if path.exists() else {"source_revision": revision()}
    report["job_status"] = status
    write_json(path, report)


def run(name, command):
    initialize(name)
    report_path = REPORTS / name / "status.json"
    omitted = 0
    with (REPORTS / name / "checks.log").open("w", encoding="utf-8") as log:
        with subprocess.Popen(command, cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                              text=True, encoding="utf-8", errors="replace", bufsize=1) as process:
            for line in process.stdout:
                cleaned = safe_line(line)
                if cleaned is None:
                    omitted += 1
                else:
                    print(cleaned, flush=True)
                    log.write(cleaned + "\n")
                    log.flush()
            code = process.wait()
    report = json.loads(report_path.read_text())
    report.update(status="passed" if code == 0 else "failed", exit_code=code,
                  omitted_diagnostic_lines=omitted, finished=datetime.now(timezone.utc).isoformat())
    write_json(report_path, report)
    return code


def checked_name(name):
    path = PurePosixPath(name)
    if not name or path.is_absolute() or ".." in path.parts or "\\" in name or ":" in name:
        raise ValueError("unsafe artifact path")
    return path


def digest(path):
    result = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            result.update(chunk)
    return result.hexdigest()


def windows_files(directory, tests=False):
    names = {"Stillus.exe", "LICENSE.txt", "Register.ps1", "dependencies.json"} if not tests else {
        "Run-Tests.ps1", "ci_diagnostics.py", "windows_test_support.ps1", "test_windows_support.ps1",
        "tests.json", "dependencies.json",
        *json.loads((directory / "tests.json").read_text()),
    }
    dependencies = json.loads((directory / "dependencies.json").read_text())
    # The build script records only DLLs actually bundled, plus the EXEs it inspected.
    by_lower = {path.name.lower(): path.name for path in directory.iterdir()}
    names.update(by_lower[name] for name in dependencies if name.endswith(".dll"))
    if (directory / "MinGW-LICENSE.txt").is_file():
        names.add("MinGW-LICENSE.txt")
    for name in names:
        if len(checked_name(name).parts) != 1:
            raise ValueError("invalid Windows package manifest")
    return [(directory / name, Path("tests") / name if tests else Path(name)) for name in sorted(names)]


def package(name):
    sha = revision()
    if name == "macos":
        directory = ROOT / "dist/Stillus.app"
        metadata = json.loads((directory / "Contents/Resources/release.json").read_text())
        if metadata["source_revision"] != sha:
            raise ValueError("macOS bundle was built from a different source revision")
        files = [(path, Path("Stillus.app") / path.relative_to(directory))
                 for path in sorted(directory.rglob("*")) if path.is_file()]
        files.append((ROOT / "LICENSE", Path("LICENSE.txt")))
        arch = "arm64"
    elif name == "linux":
        arch = platform.machine()
        directory = ROOT / "dist/linux" / arch
        files = [(directory / item, Path(item)) for item in (
            "stillus", "stillus.svg", "Register.py", "org.stillus.Stillus.desktop", "LICENSE.txt")]
    else:
        arch = "x86_64"
        directory = ROOT / "dist/windows/x86_64"
        files = windows_files(directory)
        if name == "windows-tests":
            files += windows_files(directory / "tests", tests=True)
        if any(path.suffix == ".dll" for path, _ in files) and not any(
            relative == Path("MinGW-LICENSE.txt") for _, relative in files
        ):
            files.append((Path("/usr/share/doc/mingw-w64-common/copyright"), Path("MinGW-LICENSE.txt")))
    destination = ARTIFACTS / name
    destination.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="stillus-ci-package-") as temporary:
        staged = Path(temporary)
        records = []
        for source, relative in files:
            checked_name(relative.as_posix())
            if source.is_symlink() or not source.is_file():
                raise ValueError("artifact input must be a regular package file")
            target = staged / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, target)
            records.append({"path": relative.as_posix(), "sha256": digest(target)})
        (staged / "SOURCE_REVISION.txt").write_text(sha + "\n", encoding="ascii")
        records.append({"path": "SOURCE_REVISION.txt", "sha256": digest(staged / "SOURCE_REVISION.txt")})
        write_json(staged / "build.json", {"source_revision": sha, "platform": name,
                                           "architecture": arch, "files": records})
        basename = f"stillus-{name}-{arch}"
        if name.startswith("windows"):
            archive = destination / (basename + ".zip")
            with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=1) as output:
                for file in sorted(staged.rglob("*")):
                    if file.is_file():
                        output.write(file, file.relative_to(staged).as_posix())
        else:
            archive = destination / (basename + ".tar.gz")
            with tarfile.open(archive, "w:gz", compresslevel=1) as output:
                output.add(staged, arcname=basename)
    print(f"CI_PACKAGE platform={name} source_revision={sha}")


def extract_windows(archive, destination, sha):
    if destination.exists():
        raise ValueError("Windows test destination must be new")
    with zipfile.ZipFile(archive) as source:
        names = source.namelist()
        if len(names) != len(set(names)):
            raise ValueError("duplicate archive members")
        for info in source.infolist():
            checked_name(info.filename)
            if stat.S_ISLNK(info.external_attr >> 16):
                raise ValueError("linked archive member")
        metadata = json.loads(source.read("build.json"))
        if metadata["platform"] != "windows-tests" or metadata["source_revision"] != sha:
            raise ValueError("Windows test package does not belong to the checked-out revision")
        expected = {record["path"] for record in metadata["files"]} | {"build.json"}
        if set(names) != expected:
            raise ValueError("unexpected Windows package members")
        source.extractall(destination)
    for record in metadata["files"]:
        if digest(destination / record["path"]) != record["sha256"]:
            raise ValueError("Windows package checksum mismatch")
    if (destination / "SOURCE_REVISION.txt").read_text().strip() != sha:
        raise ValueError("Windows package source SHA mismatch")


def windows(archive):
    destination = ROOT / ".ci/windows"
    extract_windows(archive, destination, revision())
    return run("windows", ["pwsh", "-NoProfile", "-File", str(destination / "tests/Run-Tests.ps1"),
                           "-CI", "-ReportDirectory", str(REPORTS / "windows")])


def validate_compose(configuration):
    service = configuration["services"]["toolchain"]
    environment = service["environment"]
    for name in ("CARGO_INCREMENTAL", "CARGO_PROFILE_DEV_DEBUG", "CARGO_PROFILE_TEST_DEBUG"):
        if environment[name] != "0":
            raise ValueError("CI must disable incremental/debug information without changing checks")
    if any("ASSERTIONS" in name or "OVERFLOW" in name for name in environment):
        raise ValueError("CI must preserve assertions and overflow checks")
    mounts = {volume["target"]: volume for volume in service["volumes"]}
    for suffix in ("registry", "git"):
        if mounts[f"/usr/local/cargo/{suffix}"]["type"] != "bind":
            raise ValueError("downloaded Cargo sources must use the CI cache directories")
    if mounts["/var/cache/stillus/target"]["type"] != "volume":
        raise ValueError("Cargo target must remain a disposable runner volume")
    if not service["build"]["cache_from"] or not service["build"]["cache_to"]:
        raise ValueError("Docker layer cache is missing")
    print("CI_COMPOSE_OK")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="action", required=True)
    for name in ("init", "finish", "package", "run"):
        command = sub.add_parser(name)
        command.add_argument("platform", choices=["linux", "macos", "windows", "windows-tests"])
        if name == "finish":
            command.add_argument("status", choices=["success", "failure", "cancelled", "skipped"])
        if name == "run":
            command.add_argument("command", nargs=argparse.REMAINDER)
    sub.add_parser("windows").add_argument("archive", type=Path)
    sub.add_parser("validate-compose")
    args = parser.parse_args()
    if args.action == "init":
        initialize(args.platform)
    elif args.action == "finish":
        finish(args.platform, args.status)
    elif args.action == "run":
        command = args.command[1:] if args.command[:1] == ["--"] else args.command
        return run(args.platform, command)
    elif args.action == "package":
        package(args.platform)
    elif args.action == "windows":
        return windows(args.archive)
    else:
        validate_compose(json.load(sys.stdin))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
