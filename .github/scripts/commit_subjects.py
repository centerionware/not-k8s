"""One policy for commit subjects and squash-merge PR titles."""

import argparse
import re
import subprocess

HEADER = re.compile(
    r"(?:build|chore|ci|docs|feat|fix|perf|refactor|revert|style|test)"
    r"(?:\([A-Za-z0-9._/-]+\))?!?: (?P<description>.+)"
)


def violations(subject):
    errors = []
    match = HEADER.fullmatch(subject)
    if not match:
        errors.append("expected type(scope): description; scope is optional")
    if len(subject) >= 100:
        errors.append(f"subject has {len(subject)} characters; maximum is 99")
    if any(ord(char) < 32 or ord(char) == 127 for char in subject):
        errors.append("control characters are not allowed")
    if "  " in subject or subject != subject.strip():
        errors.append("repeated or surrounding whitespace is not allowed")
    if subject.endswith("."):
        errors.append("subject must not end with a period")
    if match:
        description = match["description"]
        if len(description) < 10:
            errors.append("description must contain at least 10 characters")
        if re.match(r"[A-Z][a-z]", description):
            errors.append("start the description in lowercase; acronyms are allowed")
        if re.fullmatch(r"wip|stuff|things|updates?|changes?|fixes?|misc|cleanup|minor|tweaks?", description):
            errors.append("describe the specific change, not a generic placeholder")
    return errors


def check(label, subject):
    errors = violations(subject)
    print(f"{'FAIL' if errors else 'PASS'} {label}: {subject!r}")
    for error in errors:
        print(f"  - {error}")
    return not errors


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--subject")
    parser.add_argument("--base")
    parser.add_argument("--head")
    args = parser.parse_args()
    if args.subject is not None:
        if args.base or args.head:
            parser.error("use --subject OR --base and --head")
        return 0 if check("PR title", args.subject) else 1
    if not args.base or not args.head:
        parser.error("provide --subject OR --base and --head")
    commits = subprocess.check_output(
        ["git", "rev-list", "--no-merges", f"{args.base}..{args.head}", "--"], text=True
    ).splitlines()
    passed = True
    for sha in commits:
        subject = subprocess.check_output(
            ["git", "show", "-s", "--format=%s", sha, "--"], text=True
        ).removesuffix("\n")
        passed = check(sha[:8], subject) and passed
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
