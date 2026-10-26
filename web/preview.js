// The toolpath viewport: the program drawn top-down on a canvas, with rulers
// in millimetres, zoom and pan. d3 provides the scales, the rulers and the
// zoom behaviour; the path itself is drawn on a canvas because a program can
// have hundreds of thousands of moves. In the browser this sets the global
// `SenderPreview`.
(function () {
  "use strict";

  var MARGIN = { left: 46, bottom: 24 }; // room for the rulers
  var PAD = 18; // space around a fitted path, in pixels

  // Builds the viewport inside `root`, which holds a canvas, an svg for the
  // rulers, and an element for the pointer readout. `onZoom` is told whether
  // the view has been zoomed away from the fitted one.
  function create(root, onZoom) {
    var canvas = root.querySelector("canvas");
    var svg = d3.select(root).select("svg.rulers");
    var readout = root.querySelector(".readout");
    var xAxis = svg.append("g").attr("class", "axis axis-x");
    var yAxis = svg.append("g").attr("class", "axis axis-y");

    var scene = null; // what to draw, from the page
    var size = { width: 0, height: 0 };
    var base = null; // {x, y}: the scales that fit the path
    var fitted = null; // the scene key and size the base scales were fitted for
    var transform = d3.zoomIdentity;
    var queued = false;

    var zoom = d3
      .zoom()
      .scaleExtent([0.25, 400])
      .on("zoom", function (event) {
        transform = event.transform;
        if (onZoom) {
          onZoom(transform.k !== 1 || transform.x !== 0 || transform.y !== 0);
        }
        schedule();
      });
    // Double click is left to the page; the Fit button resets the view.
    d3.select(canvas).call(zoom).on("dblclick.zoom", null);

    canvas.addEventListener("pointermove", function (event) {
      if (!base) {
        return;
      }
      var box = canvas.getBoundingClientRect();
      var x = transform.rescaleX(base.x).invert(event.clientX - box.left);
      var y = transform.rescaleY(base.y).invert(event.clientY - box.top);
      readout.textContent = "X " + x.toFixed(2) + "  Y " + y.toFixed(2);
    });
    canvas.addEventListener("pointerleave", function () {
      readout.textContent = "";
    });

    function schedule() {
      if (queued) {
        return;
      }
      queued = true;
      window.requestAnimationFrame(function () {
        queued = false;
        render();
      });
    }

    // Scales with one millimetre the same length on both axes, centred on
    // the path (and the tool, when it is elsewhere).
    function fit() {
      var p = scene.parsed;
      var lo = [p.min[0], p.min[1]];
      var hi = [p.max[0], p.max[1]];
      if (scene.tool) {
        lo = [Math.min(lo[0], scene.tool[0]), Math.min(lo[1], scene.tool[1])];
        hi = [Math.max(hi[0], scene.tool[0]), Math.max(hi[1], scene.tool[1])];
      }
      var plotW = Math.max(size.width - MARGIN.left - 2 * PAD, 10);
      var plotH = Math.max(size.height - MARGIN.bottom - 2 * PAD, 10);
      var spanX = Math.max(hi[0] - lo[0], 1);
      var spanY = Math.max(hi[1] - lo[1], 1);
      var k = Math.min(plotW / spanX, plotH / spanY);
      var cx = (lo[0] + hi[0]) / 2;
      var cy = (lo[1] + hi[1]) / 2;
      var midX = MARGIN.left + (size.width - MARGIN.left) / 2;
      var midY = (size.height - MARGIN.bottom) / 2;
      base = {
        x: d3.scaleLinear().domain([cx - midX / k, cx + (size.width - midX) / k]).range([0, size.width]),
        y: d3.scaleLinear().domain([cy - (size.height - midY) / k, cy + midY / k]).range([size.height, 0]),
      };
      fitted = { key: scene.key, width: size.width, height: size.height };
    }

    // Whether a point is inside the area the base scales show.
    function inView(point) {
      var x = base.x(point[0]);
      var y = base.y(point[1]);
      return x >= MARGIN.left && x <= size.width && y >= 0 && y <= size.height - MARGIN.bottom;
    }

    function resetZoom() {
      d3.select(canvas).call(zoom.transform, d3.zoomIdentity);
    }

    function render() {
      var box = root.getBoundingClientRect();
      size = { width: Math.round(box.width), height: Math.round(box.height) };
      if (!scene || size.width === 0 || size.height === 0) {
        return;
      }
      var ratio = window.devicePixelRatio || 1;
      if (canvas.width !== size.width * ratio || canvas.height !== size.height * ratio) {
        canvas.width = size.width * ratio;
        canvas.height = size.height * ratio;
      }
      svg.attr("viewBox", "0 0 " + size.width + " " + size.height);
      var ctx = canvas.getContext("2d");
      ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
      ctx.clearRect(0, 0, size.width, size.height);
      var c = scene.palette;

      var p = scene.parsed;
      if (!p || !p.segments.length) {
        base = null;
        xAxis.selectAll("*").remove();
        yAxis.selectAll("*").remove();
        drawGrid(ctx, null, null, c);
        return;
      }

      // A new program, or a resized viewport, gets fitted again. So does a
      // tool that has left the fitted view while nobody has zoomed.
      var zoomed = transform.k !== 1 || transform.x !== 0 || transform.y !== 0;
      if (!fitted || fitted.key !== scene.key) {
        fit();
        resetZoom();
      } else if (fitted.width !== size.width || fitted.height !== size.height) {
        fit();
      } else if (!zoomed && scene.tool && !inView(scene.tool)) {
        fit();
      }

      var x = transform.rescaleX(base.x);
      var y = transform.rescaleY(base.y);
      drawGrid(ctx, x, y, c);

      ctx.save();
      ctx.beginPath();
      ctx.rect(MARGIN.left, 0, size.width - MARGIN.left, size.height - MARGIN.bottom);
      ctx.clip();

      if (scene.bounds) {
        drawBounds(ctx, x, y, scene.bounds, c);
      }
      drawOrigin(ctx, x(0), y(0), c);

      var done = scene.done;
      var rapid = [];
      var pending = [];
      var finished = [];
      p.segments.forEach(function (s) {
        (s.rapid ? rapid : s.line <= done ? finished : pending).push(s);
      });
      stroke(ctx, x, y, rapid, c.rapid, 1, [2, 4]);
      stroke(ctx, x, y, pending, c.pending, 1.5, []);
      stroke(ctx, x, y, finished, c.done, 2.25, []);
      if (scene.tool) {
        drawTool(ctx, x(scene.tool[0]), y(scene.tool[1]), c);
      }
      ctx.restore();

      drawRulers(x, y);
    }

    function drawGrid(ctx, x, y, c) {
      ctx.fillStyle = c.bg;
      ctx.fillRect(0, 0, size.width, size.height);
      if (!x) {
        return;
      }
      var xs = x.ticks(Math.max(2, Math.round(size.width / 70)));
      var ys = y.ticks(Math.max(2, Math.round(size.height / 70)));
      // A finer grid between the ruler marks.
      var minorX = x.ticks(Math.max(2, Math.round(size.width / 14)));
      var minorY = y.ticks(Math.max(2, Math.round(size.height / 14)));
      lines(ctx, x, y, minorX, minorY, c.gridMinor);
      lines(ctx, x, y, xs, ys, c.grid);
    }

    function lines(ctx, x, y, xs, ys, color) {
      ctx.beginPath();
      xs.forEach(function (v) {
        var px = Math.round(x(v)) + 0.5;
        ctx.moveTo(px, 0);
        ctx.lineTo(px, size.height - MARGIN.bottom);
      });
      ys.forEach(function (v) {
        var py = Math.round(y(v)) + 0.5;
        ctx.moveTo(MARGIN.left, py);
        ctx.lineTo(size.width, py);
      });
      ctx.strokeStyle = color;
      ctx.lineWidth = 1;
      ctx.stroke();
    }

    function stroke(ctx, x, y, segments, color, width, dash) {
      if (!segments.length) {
        return;
      }
      ctx.save();
      ctx.setLineDash(dash);
      ctx.strokeStyle = color;
      ctx.lineWidth = width;
      ctx.lineJoin = "round";
      ctx.lineCap = "round";
      ctx.beginPath();
      segments.forEach(function (s) {
        ctx.moveTo(x(s.a[0]), y(s.a[1]));
        ctx.lineTo(x(s.b[0]), y(s.b[1]));
      });
      ctx.stroke();
      ctx.restore();
    }

    // The boundaries as a hazard line: alternating yellow and black dashes.
    function drawBounds(ctx, x, y, b, c) {
      var left = x(b.x[0]);
      var top = y(b.y[1]);
      var w = x(b.x[1]) - left;
      var h = y(b.y[0]) - top;
      ctx.save();
      ctx.lineWidth = 3;
      ctx.setLineDash([9, 9]);
      ctx.strokeStyle = c.hazardDark;
      ctx.strokeRect(left, top, w, h);
      ctx.lineDashOffset = 9;
      ctx.strokeStyle = c.hazard;
      ctx.strokeRect(left, top, w, h);
      ctx.restore();
    }

    // The work origin, with X and Y arrows.
    function drawOrigin(ctx, ox, oy, c) {
      var len = 22;
      ctx.save();
      ctx.lineWidth = 2;
      arrow(ctx, ox, oy, ox + len, oy, c.axisX);
      arrow(ctx, ox, oy, ox, oy - len, c.axisY);
      ctx.beginPath();
      ctx.arc(ox, oy, 3, 0, 2 * Math.PI);
      ctx.fillStyle = c.text;
      ctx.fill();
      ctx.restore();
    }

    function arrow(ctx, x0, y0, x1, y1, color) {
      var angle = Math.atan2(y1 - y0, x1 - x0);
      ctx.strokeStyle = color;
      ctx.fillStyle = color;
      ctx.beginPath();
      ctx.moveTo(x0, y0);
      ctx.lineTo(x1, y1);
      ctx.stroke();
      ctx.beginPath();
      ctx.moveTo(x1 + 2 * Math.cos(angle), y1 + 2 * Math.sin(angle));
      ctx.lineTo(x1 - 6 * Math.cos(angle - 0.5), y1 - 6 * Math.sin(angle - 0.5));
      ctx.lineTo(x1 - 6 * Math.cos(angle + 0.5), y1 - 6 * Math.sin(angle + 0.5));
      ctx.closePath();
      ctx.fill();
    }

    // The tool: crosshairs across the view, and a ring where it is.
    function drawTool(ctx, tx, ty, c) {
      ctx.save();
      ctx.strokeStyle = c.tool;
      ctx.globalAlpha = 0.35;
      ctx.lineWidth = 1;
      ctx.setLineDash([4, 4]);
      ctx.beginPath();
      ctx.moveTo(MARGIN.left, ty);
      ctx.lineTo(size.width, ty);
      ctx.moveTo(tx, 0);
      ctx.lineTo(tx, size.height - MARGIN.bottom);
      ctx.stroke();
      ctx.globalAlpha = 1;
      ctx.setLineDash([]);
      ctx.lineWidth = 2;
      ctx.beginPath();
      ctx.arc(tx, ty, 7, 0, 2 * Math.PI);
      ctx.stroke();
      ctx.beginPath();
      ctx.arc(tx, ty, 2.5, 0, 2 * Math.PI);
      ctx.fillStyle = c.tool;
      ctx.fill();
      ctx.restore();
    }

    // Rulers along the bottom and left, with marks only where the plot is.
    function drawRulers(x, y) {
      var xs = x.ticks(Math.max(2, Math.round(size.width / 70))).filter(function (v) {
        return x(v) >= MARGIN.left;
      });
      var ys = y.ticks(Math.max(2, Math.round(size.height / 70))).filter(function (v) {
        return y(v) <= size.height - MARGIN.bottom;
      });
      xAxis
        .attr("transform", "translate(0," + (size.height - MARGIN.bottom) + ")")
        .call(d3.axisBottom(x).tickValues(xs).tickSizeOuter(0));
      yAxis
        .attr("transform", "translate(" + MARGIN.left + ",0)")
        .call(d3.axisLeft(y).tickValues(ys).tickSizeOuter(0));
    }

    return {
      // Draws `next`: {key, parsed, done, tool, bounds, palette}. `key`
      // names the program; a new one is fitted to the view.
      draw: function (next) {
        scene = next;
        schedule();
      },
      // Back to the fitted view.
      fit: function () {
        fitted = null;
        schedule();
      },
      zoomBy: function (factor) {
        d3.select(canvas).transition().duration(150).call(zoom.scaleBy, factor);
      },
    };
  }

  window.SenderPreview = { create: create };
})();
