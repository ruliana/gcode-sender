// Run with: node --test web/lib.test.js
const test = require("node:test");
const assert = require("node:assert/strict");
const lib = require("./lib.js");

const near = (a, b, eps = 1e-6) => assert.ok(Math.abs(a - b) <= eps, `${a} is not within ${eps} of ${b}`);

function radius(p, cx, cy) {
  return Math.hypot(p[0] - cx, p[1] - cy);
}

test("straight moves become segments with their file line", () => {
  const p = lib.parseProgram("G21 G90\nG0 X0 Y0\nG1 X10 Y0 F600\n; note\nG1 X10 Y5\n");
  assert.equal(p.segments.length, 2);
  assert.deepEqual(p.segments[0].b, [10, 0, 0]);
  assert.equal(p.segments[0].line, 3);
  assert.equal(p.segments[1].line, 5);
  assert.deepEqual(p.min, [0, 0, 0]);
  assert.deepEqual(p.max, [10, 5, 0]);
});

test("a start the program never gave is not drawn from the origin", () => {
  // G0 Z5 sets only Z; X and Y are wherever the machine happens to be.
  const p = lib.parseProgram("G21 G90\nG0 Z5\nG0 X10 Y10\nG1 Z-1 F300\nG1 X20\n");
  assert.deepEqual(p.min, [10, 10, -1]);
  assert.deepEqual(p.max, [20, 10, 5]);
  assert.equal(p.segments.length, 2);
  // An arc whose plane start is unknown has no centre to follow.
  assert.equal(lib.parseProgram("G3 X0 Y10 I-10 J0\n").segments.length, 0);
});

test("the size of the cut leaves out rapids, but the boundaries do not", () => {
  const p = lib.parseProgram("G0 X50 Y50\nG1 X60 Y55 F600\nG0 X0 Y0\n");
  assert.deepEqual(p.cutMin, [50, 50, 0]);
  assert.deepEqual(p.cutMax, [60, 55, 0]);
  assert.deepEqual(p.min, [0, 0, 0]);
  assert.deepEqual(lib.parseProgram("G0 X1 Y1\nG0 X2 Y2\n").cutMin, null);
});

test("rapids are marked, cuts are not", () => {
  const p = lib.parseProgram("G0 X0 Y0 Z5\nG0 X5\nG1 Z-1 F100\n");
  assert.deepEqual(p.segments.map((s) => s.rapid), [true, false]);
});

test("comments, blank lines, and percent signs are ignored but counted", () => {
  const p = lib.parseProgram("%\n(setup)\n\nG0 X0 Y0 (start)\nG1 X1 ; go\n%\n");
  assert.equal(p.segments.length, 1);
  assert.equal(p.segments[0].line, 5);
});

test("inches are converted to millimetres", () => {
  const p = lib.parseProgram("G20 G90\nG0 X0 Y0\nG1 X1 Y2\n");
  near(p.max[0], 25.4);
  near(p.max[1], 50.8);
});

test("incremental moves add to the position", () => {
  const p = lib.parseProgram("G90 G0 X10 Y10\nG91\nG1 X5 Y-5\nG1 X5\n");
  assert.deepEqual(p.segments[0].b, [15, 5, 0]);
  assert.deepEqual(p.segments[1].b, [20, 5, 0]);
});

test("a counter-clockwise quarter circle keeps its radius and ends on target", () => {
  // From (10,0) to (0,10) around the origin.
  const p = lib.parseProgram("G0 X10 Y0\nG3 X0 Y10 I-10 J0 F500\n");
  assert.ok(p.segments.length >= 18);
  p.segments.forEach((s) => near(radius(s.b, 0, 0), 10, 1e-6));
  assert.deepEqual(p.segments.at(-1).b, [0, 10, 0]);
  // It bulges toward +X +Y, not through the origin side.
  const mid = p.segments[Math.floor(p.segments.length / 2)].b;
  assert.ok(mid[0] > 5 && mid[1] > 5, `midpoint ${mid}`);
});

test("a clockwise arc goes the other way round", () => {
  // From (10,0) to (0,10) clockwise around the origin is the long way.
  const p = lib.parseProgram("G0 X10 Y0\nG2 X0 Y10 I-10 J0 F500\n");
  const mid = p.segments[Math.floor(p.segments.length / 2)].b;
  assert.ok(mid[0] < 0 && mid[1] < 0, `midpoint ${mid}`);
  assert.ok(p.min[0] <= -9.99 && p.min[1] <= -9.99);
});

test("a full circle is drawn as a full circle", () => {
  const p = lib.parseProgram("G0 X10 Y0\nG3 X10 Y0 I-10 J0\n");
  near(p.min[0], -10, 0.05);
  near(p.max[1], 10, 0.05);
});

test("radius arcs choose the short way for a positive radius", () => {
  // Counter-clockwise from (10,0) to (0,10) with R10: centre at the origin.
  const ccw = lib.parseProgram("G0 X10 Y0\nG3 X0 Y10 R10\n");
  ccw.segments.forEach((s) => near(radius(s.b, 0, 0), 10, 1e-6));
  // Clockwise with R10: the short arc has its centre at (10,10).
  const cw = lib.parseProgram("G0 X10 Y0\nG2 X0 Y10 R10\n");
  cw.segments.forEach((s) => near(radius(s.b, 10, 10), 10, 1e-6));
  // A negative radius takes the long way, so the centre flips.
  const long = lib.parseProgram("G0 X10 Y0\nG3 X0 Y10 R-10\n");
  long.segments.forEach((s) => near(radius(s.b, 10, 10), 10, 1e-6));
});

test("helical arcs move Z evenly", () => {
  const p = lib.parseProgram("G0 X10 Y0 Z0\nG3 X0 Y10 Z-2 I-10 J0\n");
  near(p.segments.at(-1).b[2], -2);
  near(p.min[2], -2);
});

test("what the preview cannot follow is reported, not hidden", () => {
  const p = lib.parseProgram("G0 X0 Y0\nG92 X5\nG53 G0 Z0\nG18\nG2 X1 Z1 I1\nG38.2 Z-5 F50\nG55\n");
  assert.ok(p.notes.some((n) => n.includes("G92")));
  assert.ok(p.notes.some((n) => n.includes("G53")));
  assert.ok(p.notes.some((n) => n.includes("outside the XY plane")));
  assert.ok(p.notes.some((n) => n.includes("Probing")));
  assert.deepEqual(p.systems, ["G55"]);
});

test("an empty program has nothing to draw", () => {
  const p = lib.parseProgram("; nothing\nM2\n");
  assert.equal(p.segments.length, 0);
  assert.equal(p.min, null);
});

test("a path outside the boundaries is named, per axis, in machine coordinates", () => {
  const p = lib.parseProgram("G0 X0 Y0 Z0\nG1 X100 Y50 Z5\n");
  const bounds = { x: [0, 800], y: [0, 400], z: [-60, 0] };
  assert.deepEqual(lib.outsideBounds(p, bounds, [0, 0, 0]).map((m) => m[0]), ["Z"]);
  // Work zero at machine X-50 moves the path left of the boundary.
  const shifted = lib.outsideBounds(p, bounds, [-50, 0, -10]);
  assert.deepEqual(shifted.map((m) => m[0]), ["X"]);
  assert.deepEqual(lib.outsideBounds(p, null, [0, 0, 0]), []);
  assert.deepEqual(lib.outsideBounds(p, { x: [-1000, 1000] }, [0, 0, 0]), []);
});

test("durations read naturally", () => {
  assert.equal(lib.formatDuration(0), "0s");
  assert.equal(lib.formatDuration(38_000), "38s");
  assert.equal(lib.formatDuration(252_000), "4m 12s");
  assert.equal(lib.formatDuration(3_900_000), "1h 05m");
});

test("elapsed time runs while active and stops when the job ends", () => {
  const running = { state: "running", started_at_ms: 1000, finished_at_ms: null };
  assert.equal(lib.elapsedMs(running, 6000), 5000);
  const done = { state: "completed", started_at_ms: 1000, finished_at_ms: 4000 };
  assert.equal(lib.elapsedMs(done, 999999), 3000);
  assert.equal(lib.elapsedMs({ state: "loaded", started_at_ms: null }, 5), null);
});

test("time left is only offered once enough has been done", () => {
  const job = (acked) => ({ state: "running", total_lines: 200, acked_lines: acked, started_at_ms: 0 });
  assert.equal(lib.remainingMs(job(5), 10_000), null);
  assert.equal(lib.remainingMs(job(200), 10_000), null);
  // 50 of 200 lines in 10 s leaves 150 lines, 30 s.
  assert.equal(lib.remainingMs(job(50), 10_000), 30_000);
  assert.equal(lib.remainingMs({ ...job(50), state: "paused" }, 10_000), null);
});

test("keys map to jogs, and shift goes ten times as far", () => {
  assert.deepEqual(lib.jogFromKey("ArrowLeft", false), { axis: "X", sign: -1, multiplier: 1 });
  assert.deepEqual(lib.jogFromKey("PageDown", true), { axis: "Z", sign: -1, multiplier: 10 });
  assert.equal(lib.jogFromKey("a", false), null);
});

test("every job state has guidance, and a lifted pause says where", () => {
  for (const state of ["loaded", "running", "paused", "stopping", "completed", "stopped", "failed", "interrupted"]) {
    assert.ok(lib.jobHint({ state }, {}).length > 10, state);
  }
  const hint = lib.jobHint({ state: "paused", paused_at: { lifted_to: 9 } }, {});
  assert.ok(hint.includes("Z 9"), hint);
});
