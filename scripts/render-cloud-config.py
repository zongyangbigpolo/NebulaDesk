#!/usr/bin/env python3
"""Render the HTTPS proxy from the same non-secret service configuration."""

import argparse
import ipaddress
import pathlib
import re
import urllib.parse


def render(config_path, template_path):
    values = {}
    for line in config_path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        key, separator, value = line.partition("=")
        if not separator or not re.fullmatch(r"NEBULA_[A-Z_]+|RUST_LOG", key):
            raise ValueError("Expected a plain KEY=value deployment setting")
        if key in values or not value or any(c.isspace() for c in value):
            raise ValueError("Duplicate, empty or whitespace-containing setting: " + key)
        values[key] = value

    public = urllib.parse.urlsplit(values["NEBULA_PUBLIC_URL"])
    if (public.scheme != "https" or not public.hostname
            or public.username is not None or public.password is not None
            or public.path not in ("", "/") or public.query or public.fragment):
        raise ValueError("NEBULA_PUBLIC_URL must be an HTTPS origin")
    if not re.fullmatch(r"[A-Za-z0-9.:-]+", public.hostname):
        raise ValueError("Unsupported public hostname")
    port = 443 if public.port is None else public.port
    if not 1 <= port <= 65535:
        raise ValueError("Invalid HTTPS port")
    listen = urllib.parse.urlsplit("http://" + values["NEBULA_LISTEN"])
    if (not listen.hostname or listen.port is None or listen.port == 0
            or listen.username is not None
            or listen.path or listen.query or listen.fragment
            or not ipaddress.ip_address(listen.hostname).is_loopback):
        raise ValueError("This single-host proxy requires a private loopback Manager listener")
    if values["NEBULA_TICKET_ISSUER"] != values["NEBULA_PUBLIC_URL"]:
        raise ValueError("Ticket issuer must match the public Manager URL")
    host = public.hostname
    if ":" in host:
        host = "[" + host + "]"
    result = template_path.read_text()
    for key, value in {
        "@PUBLIC_HOST@": host,
        "@HTTPS_PORT@": str(port),
        "@MANAGER_UPSTREAM@": "http://" + listen.netloc,
    }.items():
        result = result.replace(key, value)
    return result


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("config", type=pathlib.Path)
    parser.add_argument("--output", required=True, type=pathlib.Path)
    parser.add_argument("--template", type=pathlib.Path,
                        default=pathlib.Path(__file__).resolve().parents[1]
                        / "deploy/cloud/nginx.conf.template")
    args = parser.parse_args()
    try:
        content = render(args.config, args.template)
    except (KeyError, ValueError) as error:
        parser.error(str(error))
    args.output.write_text(content)
