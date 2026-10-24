#!/usr/bin/env python3
"""Generate a CycloneDX 1.5 SBOM from Cargo metadata without extra tools."""

import json
import subprocess
import sys
import uuid
from datetime import datetime, timezone
from pathlib import Path


def main() -> int:
    output = Path(sys.argv[1] if len(sys.argv) > 1 else "target/thais-sbom.cdx.json")
    metadata = json.loads(
        subprocess.check_output(
            ["cargo", "metadata", "--format-version", "1", "--locked"],
            text=True,
        )
    )
    packages = {package["id"]: package for package in metadata["packages"]}
    resolve = metadata.get("resolve") or {}
    nodes = {node["id"]: node for node in resolve.get("nodes", [])}
    root_id = resolve.get("root")
    root_package = packages.get(root_id) if root_id else None
    cli = next((package for package in packages.values() if package["name"] == "thais-cli"), None)
    version = (cli or root_package or {}).get("version", "0.0.0")

    def purl(package: dict) -> str:
        return f"pkg:cargo/{package['name']}@{package['version']}"

    components = []
    seen = set()
    for package in sorted(metadata["packages"], key=lambda item: (item["name"], item["version"])):
        ref = purl(package)
        if ref in seen:
            continue
        seen.add(ref)
        component = {
            "type": "library",
            "bom-ref": ref,
            "name": package["name"],
            "version": package["version"],
            "purl": ref,
            "scope": "required",
        }
        if package.get("license"):
            component["licenses"] = [{"license": {"name": package["license"]}}]
        components.append(component)

    dependencies = []
    for node_id, node in nodes.items():
        package = packages.get(node_id)
        if not package:
            continue
        dependencies.append(
            {
                "ref": purl(package),
                "dependsOn": sorted(
                    purl(packages[dep])
                    for dep in node.get("dependencies", [])
                    if dep in packages
                ),
            }
        )

    timestamp = datetime.fromtimestamp(
        int(__import__("os").environ.get("SOURCE_DATE_EPOCH", datetime.now().timestamp())),
        tz=timezone.utc,
    ).isoformat()
    document = {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "serialNumber": f"urn:uuid:{uuid.uuid5(uuid.NAMESPACE_URL, f'thais-sbom-{version}')}",
        "version": 1,
        "metadata": {
            "timestamp": timestamp,
            "tools": [{"vendor": "Thais", "name": "scripts/sbom.py", "version": "1"}],
            "component": {
                "type": "application",
                "bom-ref": f"pkg:cargo/thais@{version}",
                "name": "thais",
                "version": version,
                "purl": f"pkg:cargo/thais@{version}",
            },
        },
        "components": components,
        "dependencies": dependencies,
    }
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(f"wrote {output} ({len(components)} components)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
