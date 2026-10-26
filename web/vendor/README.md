# Vendored files

Third-party files the web UI uses, copied here unchanged so the UI works on a
machine with no internet access. They are compiled into the sender binary
with the rest of `web/` (see `crates/gcode-sender/src/web.rs`).

| File | From | License |
| --- | --- | --- |
| `alpine-csp-3.17.5.min.js` | npm `@alpinejs/csp@3.17.5`, `dist/cdn.min.js` | MIT, `licenses/alpinejs-LICENSE.txt` |
| `d3-7.9.0.min.js` | npm `d3@7.9.0`, `dist/d3.min.js` | ISC, `licenses/d3-LICENSE.txt` |
| `fonts/DSEG7Classic-Bold.woff2` | npm `dseg@0.46.0`, `fonts/DSEG7-Classic/` | OFL 1.1, `licenses/DSEG-LICENSE.txt` |
| `fonts/barlow-condensed-latin-{500,700}-normal.woff2` | npm `@fontsource/barlow-condensed@5.3.0`, `files/` | OFL 1.1, `licenses/BarlowCondensed-LICENSE.txt` |

Alpine is the CSP build, which evaluates template expressions with its own
parser instead of `eval`, so the page keeps its `script-src 'self'` policy.

To update one, replace the file, change its name and the table above, and
change the path in `web/index.html` and `web.rs`.
