#!/usr/bin/env python3
"""Checks the `windows-behaviour` filter in ci.yml against the modules it claims.

`cargo test -- <filter>` matches each filter token as a *substring* of the full
test name, `<module path>::tests::<test function>`. Two things follow, and only
the first is obvious:

  * a token is broader than it looks - `state::` also matches every test in
    `ui_state`, because `ui_state` ends in `state`; and
  * a token quietly stops covering a module that was added or renamed, and
    **nothing fails** - a job written to run 250 tests runs 167 and reports
    success. Out of 900 tests, a filter that lost a module is invisible.

So the job's intent is written down instead of inferred:
`windows-test-modules.txt` lists the modules the job is for, and this script
checks three properties against the source tree, with no build required.

  1. every module in the list exists and has tests - a rename cannot leave the
     list pointing at nothing;
  2. every module in the list is selected by the filter - the property that
     fails silently otherwise, and the reason to run this;
  3. every token in the filter selects at least one test - a token for a module
     that no longer exists is dead weight that looks like coverage.

Usage: check-ci-filter.py [--workflow PATH] [--modules PATH]
Exit status is 1 when any of the three fails, 0 when all hold.
"""

from __future__ import annotations

import argparse
import os
import re
import sys

REPOSITORY = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
DIRECTORY = os.path.join(REPOSITORY, "scripts", "local-check")
DEFAULT_WORKFLOW = os.path.join(REPOSITORY, ".github", "workflows", "ci.yml")
DEFAULT_MODULES = os.path.join(DIRECTORY, "windows-test-modules.txt")

# The job whose filter is checked.
JOB = "windows-behaviour"

# Steps of that job which are not the filtered `cargo test`: the lifecycle step
# filters on `session::` and the CLI step builds and runs the binary. Only the
# multi-token filter is checked, so the tokens are taken from the command with
# the most of them.
def read(path: str) -> str:
    with open(path, encoding="utf-8") as handle:
        return handle.read()


def job_block(workflow: str, name: str) -> str:
    """One job's text, from its `name:` banner to the next job at that indent."""
    lines = workflow.splitlines()
    for index, line in enumerate(lines):
        if re.fullmatch(r"  %s:\s*" % re.escape(name), line):
            break
    else:
        raise SystemExit(f"no job named {name!r} in the workflow")

    block = []
    for line in lines[index + 1 :]:
        if re.fullmatch(r"  [A-Za-z_][\w-]*:\s*", line):
            break
        block.append(line)
    return "\n".join(block)


def commands(block: str) -> list[str]:
    """The job's shell, with `run: >` folded line breaks removed.

    Continuation lines are indented past the `- name:` key (six spaces);
    joining on that indent reconstructs what the runner executes.
    """
    folded: list[str] = []
    for line in block.splitlines():
        indent = len(line) - len(line.lstrip())
        if indent >= 8 and folded:
            folded[-1] = folded[-1] + " " + line.strip()
        else:
            folded.append(line.strip())
    return folded


def filters(block: str) -> list[list[str]]:
    """Every `cargo test ... -- <tokens>` command's filter, as token lists."""
    found: list[list[str]] = []
    for command in commands(block):
        if "cargo test" not in command or " -- " not in command:
            continue
        tokens = [
            token
            for token in command.split(" -- ", 1)[1].split()
            if re.fullmatch(r"[\w:]+", token)
        ]
        if tokens:
            found.append(tokens)
    return found


def declared(path: str) -> list[str]:
    modules = []
    for line in read(path).splitlines():
        text = line.split("#", 1)[0].strip()
        if text:
            modules.append(text.split()[0])
    return modules


def test_modules(root: str) -> dict[str, list[str]]:
    """`module path` -> test function names, derived from the source tree.

    The path is what `cargo test` prints in front of the function name: the
    module's path inside its crate with `src/` stripped. `crates/lambo-core/
    src/ui_state/tests.rs` yields `ui_state::tests`; a `mod tests { .. }` block
    inside `foo.rs` yields `foo::tests`; `crates/lambo-cli/tests/cli_php.rs`
    yields `cli_php`, because integration tests are their own crate.
    """
    found: dict[str, list[str]] = {}

    def record(path: str, text: str) -> None:
        functions = re.findall(r"#\[test\]\s*(?:#\[[^\]]*\]\s*)*fn\s+(\w+)", text)
        if functions:
            found[path] = functions

    for crate in sorted(os.listdir(os.path.join(root, "crates"))):
        base = os.path.join(root, "crates", crate)
        for source, integration in ((os.path.join(base, "src"), False),
                                    (os.path.join(base, "tests"), True)):
            if not os.path.isdir(source):
                continue
            for directory, _, files in os.walk(source):
                if "fixtures" in directory:
                    continue
                for name in sorted(files):
                    if not name.endswith(".rs"):
                        continue
                    text = read(os.path.join(directory, name))
                    relative = os.path.relpath(os.path.join(directory, name), source)
                    parts = relative[: -len(".rs")].split(os.sep)
                    if parts[-1] == "mod":
                        parts = parts[:-1]
                    # A separate `tests.rs` or `tests/mod.rs` is its own module.
                    if parts[-1] == "tests":
                        record("::".join(parts), text)
                        continue
                    # An inline `mod tests { .. }` runs to the end of the file,
                    # so searching from the declaration is enough.
                    index = text.find("mod tests")
                    if index != -1:
                        record("::".join(parts + ["tests"]), text[index:])
    return found


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workflow", default=DEFAULT_WORKFLOW)
    parser.add_argument("--modules", default=DEFAULT_MODULES)
    arguments = parser.parse_args()

    block = job_block(read(arguments.workflow), JOB)
    candidates = filters(block)
    if not candidates:
        raise SystemExit(f"no filtered `cargo test` in the {JOB} job")
    tokens = max(candidates, key=len)

    modules = test_modules(REPOSITORY)
    names = {
        f"{module}::{function}"
        for module, functions in modules.items()
        for function in functions
    }
    wanted = declared(arguments.modules)

    print(f"{JOB}: {len(tokens)} filter tokens, {len(names)} tests in the tree")
    print(f"declared coverage: {len(wanted)} modules, from "
          f"{os.path.relpath(arguments.modules, REPOSITORY)}")

    failures: list[str] = []

    print("\n1. every declared module exists and has tests")
    for module in wanted:
        count = sum(
            len(functions)
            for path, functions in modules.items()
            if path == module or path.startswith(module + "::")
        )
        if count:
            print(f"   ok      {module:<12} {count:>4} tests")
        else:
            print(f"   MISSING {module:<12} no tests in the tree")
            failures.append(f"{module} is declared but has no tests")

    print("\n2. every declared module is selected by the filter")
    for module in wanted:
        selected = [
            name
            for name in names
            if (name == module or name.startswith(module + "::"))
            and any(token in name for token in tokens)
        ]
        total = sum(
            len(functions)
            for path, functions in modules.items()
            if path == module or path.startswith(module + "::")
        )
        if len(selected) == total:
            continue
        print(f"   MISSED  {module:<12} {len(selected)}/{total} selected "
              f"({total - len(selected)} tests do not run)")
        failures.append(f"{module} has {total - len(selected)} tests the filter misses")

    if not failures:
        print("   ok      every declared module is selected")

    print("\n3. every token selects at least one test")
    for token in tokens:
        count = sum(1 for name in names if token in name)
        if count:
            print(f"   ok      {token:<12} {count:>4} tests")
        else:
            print(f"   DEAD    {token:<12} matches nothing")
            failures.append(f"{token} matches no test")

    if not failures:
        print("\nclean: the filter matches the modules the job is for")
        return 0

    print("\n" + "\n".join(f"  - {failure}" for failure in failures))
    print(
        "\nNothing fails on its own when this happens: `cargo test` with a filter\n"
        "that matches less than it should still exits zero. That is why this is a\n"
        "script and not a passing test. Fix the filter in "
        ".github/workflows/ci.yml -\n"
        "or, if the job is not meant to cover a module, remove it from "
        "windows-test-modules.txt."
    )
    return 1


if __name__ == "__main__":
    sys.exit(main())
