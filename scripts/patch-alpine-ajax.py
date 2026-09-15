#!/usr/bin/env python3
"""Reapply Rostra's transport-failure fix to the vendored Alpine AJAX 0.12.6 bundle.

Run after replacing the bundle with its upstream distribution. Keep the patch
until upstream dispatches failure/completion events and releases cached requests
and busy targets when fetch or response-body reading rejects.
"""

from pathlib import Path

bundle = (
    Path(__file__).resolve().parent.parent
    / "crates/rostra-web-ui/assets/libs/alpine-ajax@0.12.6.js"
)
original = (
    "await c.then(u=>{r.ok=u.ok,r.redirected=u.redirected,r.url=u.url,"
    "r.status=u.status,r.html=u.html,r.raw=u.raw})"
)
patched = original + (
    ".catch(u=>{h.purge(r);x.delete(o.action);r.status=0;"
    'd(e.el,"ajax:error",r);d(e.el,"ajax:after",{response:r,render:[]});throw u})'
)
source = bundle.read_text()
if patched not in source:
    if source.count(original) != 1:
        raise SystemExit("Unexpected Alpine AJAX bundle; re-evaluate the patch")
    bundle.write_text(source.replace(original, patched))
