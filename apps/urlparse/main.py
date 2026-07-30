#!/usr/bin/env python3
"""RFC-3986 URL/URI dissector via stdlib urllib.parse. Pure, offline."""
import os
import sys
from urllib.parse import urlsplit, urlunsplit, parse_qsl

# Shared host-link plumbing (socket loop, token stamping, frame bound, the
# agent-tool id echo) from apps/_sdk — fs_read-granted. The path is resolved
# relative to THIS file (apps/<app>/main.py -> ../_sdk), so it works both when
# darwind launches the app (cwd = project root) and when the tests run from the
# app dir. Bytecode writes are disabled since apps/_sdk is read-only in the
# sandbox. Re-importing drain_lines/MAX_FRAME_BYTES/TOKEN keeps them resolvable
# off `main` for the framing/contract tests.
sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "_sdk"))
from harness import (  # noqa: E402 — must follow the sys.path insert above
    MAX_FRAME_BYTES,
    TOKEN,
    drain_lines,
    reply_result,
    run,
    send,
)

MAX_PARAMS = 1000  # listed query params; param_count/params_truncated carry the rest


def compute(payload):
    """PURE, offline, no I/O, never raises. Dissect an RFC-3986 URL/URI.

    Input: payload["url"] (non-empty str). Never fetches — parsing only.
    Output dict: scheme, host, port (explicit else the scheme default for
    http=80/https=443/ftp=21/ssh=22/ws=80/wss=443 else null), path, query,
    fragment, userinfo_present (bool), params ([{key,value}] from parse_qsl
    keep_blank_values=True), is_idn (host has non-ASCII), host_punycode
    (host.encode("idna") decoded, or null when N/A), normalized (scheme +
    host lowercased, default port dropped) and warnings ([...]). A URL with
    no scheme still parses (scheme=""). Empty/non-str url -> {"error": ...}."""
    try:
        if not isinstance(payload, dict):
            return {"error": "payload must be a mapping"}
        url = payload.get("url")
        if not isinstance(url, str):
            return {"error": "url must be a string"}
        if not url.strip():
            return {"error": "url must be non-empty"}

        sr = urlsplit(url)
        scheme = sr.scheme  # urlsplit lowercases the scheme
        host = sr.hostname or ""  # lowercased; brackets stripped for IPv6

        defaults = {"http": 80, "https": 443, "ftp": 21,
                    "ssh": 22, "ws": 80, "wss": 443}

        try:
            explicit_port = sr.port
        except ValueError:
            # The URL DOES carry an explicit port token, but it is non-numeric
            # or out of 0-65535. Reporting the scheme default here would be a
            # false answer (and "normalized" would silently rewrite the
            # authority to a different origin) — refuse honestly instead.
            return {"error": "invalid explicit port in URL authority (must be 0-65535)"}

        if explicit_port is not None:
            port = explicit_port
        else:
            port = defaults.get(scheme)  # None when scheme has no known default

        userinfo_present = sr.username is not None or sr.password is not None

        pairs = parse_qsl(sr.query, keep_blank_values=True)
        # Bounded params: an unbounded list can push the reply past the
        # daemon's 1 MiB app-line budget (measured ~1.44 MB at 40k pairs), so
        # the full count + a truncation flag carry the honest remainder.
        params = [{"key": k, "value": v} for k, v in pairs[:MAX_PARAMS]]

        is_idn = any(ord(c) > 127 for c in host)

        # IDNA2008 via the `idna` package, NOT str.encode("idna").
        #
        # The stdlib codec implements IDNA2003, whose mapping is LOSSY in a way that
        # changes which site you reach: it folds U+00DF to "ss", so faß.de came back
        # as "fass.de" — a DIFFERENT, separately-registrable domain. Reporting that
        # as the punycode form of what the user typed is a wrong answer with a
        # phishing shape, since confusable pairs are exactly what an attacker picks.
        # Under IDNA2008 faß.de is xn--fa-hia.de and stays distinct.
        host_punycode = None
        host_idna_note = None
        # An IP literal is not a domain name and must never go through IDNA - an
        # IPv6 host like 2001:db8::1 is not a valid IDNA label and would come back
        # as None with a spurious "not valid IDNA2008" note.
        _is_ip_literal = (
            ":" in host
            or (host.replace(".", "").isdigit() and host.count(".") == 3)
        )
        if host and _is_ip_literal:
            host_punycode = host
        elif host:
            try:
                import idna as _idna

                host_punycode = _idna.encode(host, uts46=True, std3_rules=False).decode("ascii")
            except ImportError:
                # No IDNA2008 available: fall back to the stdlib codec, but VERIFY the
                # round trip and refuse to report a mapping that does not survive it.
                try:
                    cand = host.encode("idna").decode("ascii")
                    if cand.encode("ascii").decode("idna") == host:
                        host_punycode = cand
                    else:
                        host_idna_note = (
                            "punycode omitted: the available IDNA2003 codec maps this "
                            "host to a different name that does not round-trip"
                        )
                except Exception:  # noqa: BLE001
                    host_punycode = None
            except Exception as exc:  # noqa: BLE001 — idna is strict by design
                host_punycode = None
                host_idna_note = f"host is not valid IDNA2008: {exc}"

        # normalized: scheme + host lowercased (already lower), default port dropped
        netloc = ""
        if userinfo_present:
            userinfo = sr.username or ""
            if sr.password is not None:
                userinfo = userinfo + ":" + sr.password
            netloc += userinfo + "@"
        if host:
            netloc += ("[" + host + "]") if ":" in host else host
        if explicit_port is not None and explicit_port != defaults.get(scheme):
            netloc += ":" + str(explicit_port)
        if netloc:
            normalized = urlunsplit((scheme, netloc, sr.path, sr.query, sr.fragment))
        else:
            # NO AUTHORITY (e.g. "https:////evil.com/a" -> host "", path
            # "//evil.com/a"). urlunsplit DROPS the empty-authority marker and then
            # re-reads the path's leading "//" as an authority, emitting
            # "https://evil.com/a" — inventing a HOST the URL never had and handing
            # a different origin to any caller using `normalized` for an origin or
            # allow-list decision. Assemble it here instead, keeping the EMPTY
            # authority ("scheme://" + "//path") whenever the path starts with "//"
            # so the host stays empty on a re-parse. Same refusal-to-rewrite-the-
            # authority stance as the bad-port branch above.
            prefix = (scheme + ":") if scheme else ""  # a relative URL has no scheme
            if sr.path.startswith("//"):
                prefix += "//"  # keep the EMPTY authority so the host stays empty
            normalized = prefix + sr.path
            if sr.query:
                normalized += "?" + sr.query
            if sr.fragment:
                normalized += "#" + sr.fragment

        warnings = []
        if userinfo_present:
            warnings.append("credentials embedded in URL")
        if scheme in ("http", "ws", "ftp"):
            warnings.append("insecure scheme (http/ws/ftp)")

        return {
            "scheme": scheme,
            "host": host,
            "port": port,
            "path": sr.path,
            "query": sr.query,
            "fragment": sr.fragment,
            "userinfo_present": userinfo_present,
            "params": params,
            "param_count": len(pairs),
            "params_truncated": len(pairs) > MAX_PARAMS,
            "is_idn": is_idn,
            "host_punycode": host_punycode,
        "host_idna_note": host_idna_note,
            "normalized": normalized,
            "warnings": warnings,
        }
    except Exception as e:  # noqa: BLE001 — compute must never raise
        return {"error": "unexpected: %s" % e}


def handle(conn, msg):
    op = msg.get("type") or msg.get("op")
    if op == "start":
        send(conn, {"type": "status", "data": {"tool": "url.dissect", "ready": True}})
    elif op == "refresh":
        send(conn, {"type": "items", "data": {"status": "ok"}})
    elif op == "url.dissect":
        reply_result(conn, msg, compute(msg))
    elif op == "stop":
        raise SystemExit(0)


if __name__ == "__main__":
    sys.exit(run(handle))
