#!/usr/bin/env python3
"""
Convert a CSV inventory file into the agents: YAML block.

─────────────────────────────────────────────────────────────────
EXPECTED CSV COLUMNS  (header row required)
─────────────────────────────────────────────────────────────────
name, os_ip, ilo_ip, dc, rack, role [, bmc_user, bmc_pass_env, bmc_insecure]

• A header row with column names is required (order doesn't matter).
• ilo_ip — leave empty to skip the bmc: block entirely.
• bmc_user / bmc_pass_env / bmc_insecure are optional columns; defaults:
      bmc_user      = Administrator
      bmc_pass_env  = HPE_ILO_PASSWORD
      bmc_insecure  = true
• Lines starting with # are treated as comments and ignored.

EXAMPLE  (hosts.csv):
─────────────────────────────────
name,os_ip,ilo_ip,dc,rack,role
test-ubuntu-26,10.232.129.18,10.240.1.193,COLO2,R12,test-ubuntu-26.04
web-server-01,192.168.1.10,,DC1,A01,web
db-primary,10.0.0.5,10.0.1.5,DC2,B03,database,svcacct,LENOVO_ILO_PASSWORD,true
─────────────────────────────────
"""

import csv
import sys
import textwrap

REQUIRED_COLS        = {"name", "os_ip", "ilo_ip", "dc", "rack", "role"}
DEFAULT_USER         = "Infra_user"
DEFAULT_BMC_USER     = "admin"
DEFAULT_BMC_PASS_ENV = "HPE_ILO_PASSWORD"
DEFAULT_BMC_INSECURE = "true"


def parse_row(row: dict, lineno: int) -> dict | None:
    """Validate and normalise one CSV row. Returns None on error."""
    missing = REQUIRED_COLS - row.keys()
    if missing:
        print(f"  [WARN] line {lineno}: missing columns {missing} — skipping", file=sys.stderr)
        return None

    name   = row["name"].strip()
    os_ip  = row["os_ip"].strip()
    ilo_ip = row["ilo_ip"].strip()

    if not name or not os_ip:
        print(f"  [WARN] line {lineno}: name/os_ip empty — skipping", file=sys.stderr)
        return None

    bmc_user     = row.get("bmc_user", "").strip()     or DEFAULT_BMC_USER
    bmc_pass_env = row.get("bmc_pass_env", "").strip() or DEFAULT_BMC_PASS_ENV
    bmc_insecure = row.get("bmc_insecure", "").strip() or DEFAULT_BMC_INSECURE

    return {
        "name":         name,
        "os_ip":        os_ip,
        "ilo_ip":       ilo_ip,
        "dc":           row["dc"].strip(),
        "rack":         row["rack"].strip(),
        "role":         row["role"].strip(),
        "bmc_user":     bmc_user,
        "bmc_pass_env": bmc_pass_env,
        "bmc_insecure": bmc_insecure.lower() in ("true", "yes", "1"),
    }


def render_agent(h: dict) -> str:
    """Return the YAML block for one agent."""
    lines = [
        f"  - name: {h['name']}",
        f"    host: {h['os_ip']}",
        f"    user: {DEFAULT_USER}",
        f"    tags:",
    ]
    if h["dc"]:
        lines.append(f"      dc: {h['dc']}")
    if h["rack"]:
        lines.append(f"      rack: {h['rack']}")
    if h["role"]:
        lines.append(f"      role: {h['role']}")

    if h["ilo_ip"]:
        insecure_val = "true" if h["bmc_insecure"] else "false"
        lines += [
            f"    bmc:",
            f"      url: \"https://{h['ilo_ip']}\"",
            f"      username: {h['bmc_user']}",
            f"      password_env: {h['bmc_pass_env']}",
            f"      insecure: {insecure_val}   # iLO uses self-signed certs",
        ]

    return "\n".join(lines)


def strip_comments(fh):
    """Yield non-comment lines so csv.DictReader ignores # lines."""
    for line in fh:
        if not line.lstrip().startswith("#"):
            yield line


def convert(input_path: str, output_path: str | None = None) -> None:
    agents  = []
    skipped = 0

    with open(input_path, newline="", encoding="utf-8") as fh:
        reader = csv.DictReader(strip_comments(fh))
        # Normalise header names: strip surrounding whitespace
        reader.fieldnames = [f.strip() for f in (reader.fieldnames or [])]

        for lineno, row in enumerate(reader, start=2):   # line 1 = header
            row  = {k.strip(): v for k, v in row.items() if k}
            host = parse_row(row, lineno)
            if host:
                agents.append(host)
            else:
                skipped += 1

    if not agents:
        print("No valid hosts found. Check your CSV file.", file=sys.stderr)
        sys.exit(1)

    blocks = ["agents:"]
    for i, h in enumerate(agents):
        blocks.append(render_agent(h))
        if i < len(agents) - 1:
            blocks.append("")           # blank line between entries

    yaml_out = "\n".join(blocks) + "\n"

    if output_path:
        with open(output_path, "w", encoding="utf-8") as fh:
            fh.write(yaml_out)
        print(f"Written {len(agents)} agent(s) to {output_path}  ({skipped} row(s) skipped)")
    else:
        print(yaml_out)
        if skipped:
            print(f"# ({skipped} row(s) skipped — see stderr)", file=sys.stderr)


# ── CLI ──────────────────────────────────────────────────────────────────────
if __name__ == "__main__":
    import argparse

    parser = argparse.ArgumentParser(
        description="Convert a CSV host inventory to agents YAML.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=textwrap.dedent("""\
            Required CSV columns (header row mandatory, column order flexible):
              name, os_ip, ilo_ip, dc, rack, role

            Optional CSV columns:
              bmc_user, bmc_pass_env, bmc_insecure

            Examples:
              python convert_agents.py hosts.csv
              python convert_agents.py hosts.csv -o agents.yaml
        """),
    )
    parser.add_argument("input", help="Path to the CSV inventory file")
    parser.add_argument("-o", "--output", help="Output YAML file (default: stdout)")
    args = parser.parse_args()

    convert(args.input, args.output)
