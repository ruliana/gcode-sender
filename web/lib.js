// Pure helpers for the web UI. They touch no DOM, so `node --test` can check
// them (web/lib.test.js). In the browser this is a plain script that sets the
// global `SenderLib`.
(function (root, factory) {
  if (typeof module === "object" && module.exports) {
    module.exports = factory();
  } else {
    root.SenderLib = factory();
  }
})(this, function () {
  "use strict";

  var WORD = /([A-Za-z])\s*([-+]?(?:\d+\.?\d*|\.\d+))/g;
  var ARC_STEP = Math.PI / 36; // arcs are drawn as chords of at most 5 degrees
  var ACTIVE = ["running", "paused", "stopping"];

  // ---- Toolpath ---------------------------------------------------------

  // Points along an arc in the XY plane from `a` to `b`, or null when the
  // line gives no usable centre or radius. `i` and `j` are the centre's offset
  // from the start; `r` is the radius form.
  function arcPoints(a, b, clockwise, i, j, r) {
    var cx;
    var cy;
    if (i !== undefined || j !== undefined) {
      cx = a[0] + (i || 0);
      cy = a[1] + (j || 0);
    } else if (r !== undefined) {
      var dx = b[0] - a[0];
      var dy = b[1] - a[1];
      var d = Math.sqrt(dx * dx + dy * dy);
      if (d === 0) {
        return null;
      }
      var h = Math.sqrt(Math.max(r * r - (d * d) / 4, 0));
      // A short arc has its centre on the left of the direction of travel
      // when it runs counter-clockwise, on the right when clockwise. A
      // negative radius asks for the long way round, which flips it.
      var left = (!clockwise && r > 0) || (clockwise && r < 0);
      var side = left ? 1 : -1;
      cx = (a[0] + b[0]) / 2 + (side * h * -dy) / d;
      cy = (a[1] + b[1]) / 2 + (side * h * dx) / d;
    } else {
      return null;
    }
    var radius = Math.sqrt(Math.pow(a[0] - cx, 2) + Math.pow(a[1] - cy, 2));
    var a0 = Math.atan2(a[1] - cy, a[0] - cx);
    var sweep = Math.atan2(b[1] - cy, b[0] - cx) - a0;
    if (clockwise) {
      while (sweep >= 0) {
        sweep -= 2 * Math.PI;
      }
    } else {
      while (sweep <= 0) {
        sweep += 2 * Math.PI;
      }
    }
    var n = Math.max(2, Math.ceil(Math.abs(sweep) / ARC_STEP));
    var points = [];
    for (var k = 0; k <= n; k++) {
      var t = k / n;
      var angle = a0 + sweep * t;
      points.push([
        cx + radius * Math.cos(angle),
        cy + radius * Math.sin(angle),
        a[2] + (b[2] - a[2]) * t,
      ]);
    }
    points[0] = a.slice();
    points[n] = b.slice();
    return points;
  }

  // Follows a program to draw it: straight moves and arcs, in the units and
  // coordinates it is written in, converted to millimetres. It is a preview,
  // not the sender's own check; `notes` lists what it could not follow.
  //
  // Each segment is `{a, b, rapid, line}` with `line` the 1-based file line,
  // so a job's `last_acked_line` says how much of the path is done.
  function parseProgram(text) {
    // Where the tool is, per axis. An axis is unknown until the program sets
    // it, since the machine can start anywhere; nothing is drawn from a
    // position the program never gave.
    var pos = [null, null, null];
    var scale = 1;
    var absolute = true;
    var plane = 17;
    var motion = null;
    var segments = [];
    var notes = [];
    var systems = {};
    var min = [Infinity, Infinity, Infinity];
    var max = [-Infinity, -Infinity, -Infinity];
    var cutMin = [Infinity, Infinity, Infinity];
    var cutMax = [-Infinity, -Infinity, -Infinity];
    var cuts = 0;

    function note(message) {
      if (notes.indexOf(message) < 0) {
        notes.push(message);
      }
    }

    function zeroIfUnknown(v) {
      return v === null ? 0 : v;
    }

    function add(a, b, rapid, line) {
      segments.push({ a: a, b: b, rapid: rapid, line: line });
      if (!rapid) {
        cuts++;
      }
      for (var i = 0; i < 3; i++) {
        min[i] = Math.min(min[i], a[i], b[i]);
        max[i] = Math.max(max[i], a[i], b[i]);
        if (!rapid) {
          cutMin[i] = Math.min(cutMin[i], a[i], b[i]);
          cutMax[i] = Math.max(cutMax[i], a[i], b[i]);
        }
      }
    }

    String(text)
      .split(/\r\n|\r|\n/)
      .forEach(function (raw, index) {
        var line = raw.replace(/\([^)]*\)/g, " ").replace(/;.*$/, "");
        if (/^\s*%/.test(line)) {
          return;
        }
        var value = {};
        var machineMove = false;
        var probe = false;
        var match;
        WORD.lastIndex = 0;
        while ((match = WORD.exec(line)) !== null) {
          var letter = match[1].toUpperCase();
          var number = parseFloat(match[2]);
          if (letter !== "G") {
            value[letter] = number;
            continue;
          }
          var g = Math.round(number * 10) / 10;
          if (g === 20) {
            scale = 25.4;
          } else if (g === 21) {
            scale = 1;
          } else if (g === 90) {
            absolute = true;
          } else if (g === 91) {
            absolute = false;
          } else if (g === 17 || g === 18 || g === 19) {
            plane = g;
          } else if (g === 0 || g === 1 || g === 2 || g === 3) {
            motion = g;
          } else if (g === 53) {
            machineMove = true;
          } else if (g >= 54 && g <= 59 && Math.floor(g) === g) {
            systems["G" + g] = true;
          } else if (g >= 38 && g < 39) {
            probe = true;
          } else if (g === 92 || g === 10 || g === 28 || g === 30) {
            note("G10, G28, G30 or G92 changes coordinates; the preview ignores it");
          }
        }

        var moves = value.X !== undefined || value.Y !== undefined || value.Z !== undefined;
        if (!moves || motion === null) {
          return;
        }
        if (machineMove) {
          note("G53 moves are in machine coordinates; the preview skips them");
          return;
        }
        if (probe) {
          note("Probing moves are not drawn");
          return;
        }

        var target = pos.slice();
        ["X", "Y", "Z"].forEach(function (axis, i) {
          if (value[axis] !== undefined) {
            var distance = value[axis] * scale;
            target[i] = absolute ? distance : (pos[i] === null ? 0 : pos[i]) + distance;
          }
        });
        // An axis the program has not set yet stays where this move puts it,
        // and one nobody has set at all is drawn at 0.
        var from = pos.map(function (v, i) {
          return v === null ? target[i] : v;
        });
        var start = from.map(zeroIfUnknown);
        var end = target.map(zeroIfUnknown);
        var knewPlanePosition = pos[0] !== null && pos[1] !== null;
        pos = target;

        var lineNo = index + 1;
        if (motion === 0 || motion === 1) {
          if (start[0] !== end[0] || start[1] !== end[1] || start[2] !== end[2]) {
            add(start, end, motion === 0, lineNo);
          }
          return;
        }
        var points = null;
        if (plane === 17) {
          // An arc needs a known start in the plane to have a centre.
          if (!knewPlanePosition) {
            return;
          }
          points = arcPoints(
            start,
            end,
            motion === 2,
            value.I === undefined ? undefined : value.I * scale,
            value.J === undefined ? undefined : value.J * scale,
            value.R === undefined ? undefined : value.R * scale
          );
        } else {
          note("Arcs outside the XY plane are drawn as straight lines");
        }
        if (points === null) {
          add(start, end, false, lineNo);
        } else {
          for (var k = 1; k < points.length; k++) {
            add(points[k - 1], points[k], false, lineNo);
          }
        }
      });

    return {
      segments: segments,
      // Everything the tool visits, rapids included: what the boundaries limit.
      min: segments.length ? min : null,
      max: segments.length ? max : null,
      // Only where it cuts: the size of the part.
      cutMin: cuts ? cutMin : null,
      cutMax: cuts ? cutMax : null,
      notes: notes,
      systems: Object.keys(systems),
    };
  }

  // Which axes of the path would leave the boundaries, as short messages.
  // `bounds` is in machine coordinates and the path in work coordinates, so
  // `zero` (the active system's zero, in machine coordinates) joins them.
  function outsideBounds(parsed, bounds, zero) {
    var out = [];
    if (!parsed || !parsed.segments.length || !bounds) {
      return out;
    }
    zero = zero || [0, 0, 0];
    ["x", "y", "z"].forEach(function (key, i) {
      var range = bounds[key];
      if (!range) {
        return;
      }
      var low = parsed.min[i] + zero[i];
      var high = parsed.max[i] + zero[i];
      if (low < range[0] - 1e-6 || high > range[1] + 1e-6) {
        out.push(
          key.toUpperCase() + " " + low.toFixed(1) + " to " + high.toFixed(1) +
            " (limit " + range[0] + " to " + range[1] + ")"
        );
      }
    });
    return out;
  }

  // ---- Time -------------------------------------------------------------

  function formatDuration(ms) {
    var total = Math.max(0, Math.round(ms / 1000));
    var hours = Math.floor(total / 3600);
    var minutes = Math.floor((total % 3600) / 60);
    var seconds = total % 60;
    function two(n) {
      return n < 10 ? "0" + n : String(n);
    }
    if (hours > 0) {
      return hours + "h " + two(minutes) + "m";
    }
    if (minutes > 0) {
      return minutes + "m " + two(seconds) + "s";
    }
    return seconds + "s";
  }

  // Time since the job started: until now while it is active, and until it
  // ended once it has. Null for a job that never started.
  function elapsedMs(job, now) {
    if (job.started_at_ms === null || job.started_at_ms === undefined) {
      return null;
    }
    var active = ACTIVE.indexOf(job.state) >= 0;
    var end = active ? now : job.finished_at_ms || now;
    return Math.max(0, end - job.started_at_ms);
  }

  // A rough time left for a running job, from how fast its lines have been
  // acknowledged so far. It is not offered until enough has been done to mean
  // something, and long moves make it only a guide.
  function remainingMs(job, now) {
    if (job.state !== "running") {
      return null;
    }
    var elapsed = elapsedMs(job, now);
    var enough = Math.max(10, job.total_lines * 0.05);
    if (!elapsed || job.acked_lines < enough || job.acked_lines >= job.total_lines) {
      return null;
    }
    return Math.round(((job.total_lines - job.acked_lines) * elapsed) / job.acked_lines);
  }

  // ---- Operator guidance ------------------------------------------------

  var KEYS = {
    ArrowLeft: ["X", -1],
    ArrowRight: ["X", 1],
    ArrowUp: ["Y", 1],
    ArrowDown: ["Y", -1],
    PageUp: ["Z", 1],
    PageDown: ["Z", -1],
  };

  // The jog a key asks for, or null. Shift makes it ten times as far.
  function jogFromKey(key, shift) {
    var jog = KEYS[key];
    return jog ? { axis: jog[0], sign: jog[1], multiplier: shift ? 10 : 1 } : null;
  }

  // One sentence on what a job's state means and what to do next.
  function jobHint(job, machine) {
    switch (job.state) {
      case "loaded":
        return "Ready. Start runs it from the first line.";
      case "running":
        return "Running. Pause lifts the tool and frees the machine for jogging. Feed hold stops where it is.";
      case "paused":
        if (job.paused_at && job.paused_at.lifted_to !== null && job.paused_at.lifted_to !== undefined) {
          return "Paused with the tool lifted to Z " + job.paused_at.lifted_to +
            ". Jog or move the work zero if you need to, then Resume brings the tool back and carries on.";
        }
        return "Held in place. Resume carries on from here.";
      case "stopping":
        return "Stopping. The machine is coming to rest.";
      case "completed":
        return "Done. Start runs it again from the top.";
      case "stopped":
        return "Stopped. Resume picks up near where it stopped, or Start runs it again from the top.";
      case "failed":
        return "It failed. Read the reason, clear any alarm with Unlock, then Resume or Start again.";
      case "interrupted":
        return "The connection dropped or the server stopped. Once the machine is connected and idle, Resume picks up where it left off.";
      default:
        return "";
    }
  }

  return {
    parseProgram: parseProgram,
    outsideBounds: outsideBounds,
    formatDuration: formatDuration,
    elapsedMs: elapsedMs,
    remainingMs: remainingMs,
    jogFromKey: jogFromKey,
    jobHint: jobHint,
  };
});
