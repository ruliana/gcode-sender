// The G-code Sender web UI. It is one page over the HTTP API that serves it,
// and it talks to nothing else. It is a plain script with no build step and
// avoids newer syntax, so it runs in older browsers. The logic that needs no
// DOM (the toolpath parser, time estimates, guidance) is in lib.js, which has
// its own tests.
(function () {
  "use strict";

  var Lib = window.SenderLib;
  var AXES = ["X", "Y", "Z"];
  var AXIS_KEYS = ["x", "y", "z"];
  var FIRMWARE_NAMES = { grbl: "Grbl", grblhal: "grblHAL" };
  var STARTABLE = ["loaded", "completed", "stopped"];
  var DELETABLE = ["loaded", "completed", "stopped", "failed", "interrupted"];
  var RESUMABLE_AFTER_STOP = ["interrupted", "stopped", "failed"];
  var ACTIVE_STATES = ["running", "paused", "stopping"];
  var BASE_TITLE = "G-code Sender";

  var model = {
    machine: null,
    jobs: [], // job records, oldest first
    selected: null, // id of the job shown in the detail panel
    settings: null, // the last settings document the server sent
    bounds: null, // the last boundaries the server sent
  };

  function $(id) {
    return document.getElementById(id);
  }

  // Builds an element. Text goes in through textContent, never innerHTML, so
  // a job name cannot inject markup.
  function h(tag, attrs) {
    var node = document.createElement(tag);
    Object.keys(attrs || {}).forEach(function (name) {
      var value = attrs[name];
      if (name === "text") {
        node.textContent = value;
      } else if (value === true) {
        node.setAttribute(name, "");
      } else if (value !== false && value !== null && value !== undefined) {
        node.setAttribute(name, value);
      }
    });
    for (var i = 2; i < arguments.length; i++) {
      node.appendChild(arguments[i]);
    }
    return node;
  }

  function setText(id, text) {
    var node = $(id);
    node.textContent = text || "";
    node.hidden = !text;
  }

  // What the operator last chose, kept in the browser. It is a convenience:
  // storage can be blocked, so every access is allowed to fail.
  function remembered(key, fallback) {
    try {
      var value = window.localStorage.getItem("gcode-sender." + key);
      return value === null ? fallback : value;
    } catch (e) {
      return fallback;
    }
  }

  function remember(key, value) {
    try {
      window.localStorage.setItem("gcode-sender." + key, String(value));
    } catch (e) {
      // Not remembered; nothing depends on it.
    }
  }

  // ---- HTTP -------------------------------------------------------------

  // Sends a request and resolves with the parsed JSON body (or null). Errors
  // from the API carry its `error` message.
  function request(method, path, body) {
    var options = { method: method, headers: {} };
    if (typeof body === "string") {
      options.body = body;
      options.headers["Content-Type"] = "text/plain; charset=utf-8";
    } else if (body !== undefined) {
      options.body = JSON.stringify(body);
      options.headers["Content-Type"] = "application/json";
    }
    return fetch(path, options).then(function (res) {
      return res.text().then(function (text) {
        var data = null;
        if (text) {
          try {
            data = JSON.parse(text);
          } catch (e) {
            data = text;
          }
        }
        if (!res.ok) {
          throw new Error(data && data.error ? data.error : "HTTP " + res.status);
        }
        return data;
      });
    });
  }

  function readFile(file) {
    return new Promise(function (resolve, reject) {
      var reader = new FileReader();
      reader.onload = function () {
        resolve(reader.result);
      };
      reader.onerror = function () {
        reject(new Error("Could not read " + file.name));
      };
      reader.readAsText(file);
    });
  }

  // ---- Feedback ---------------------------------------------------------

  var noticeTimer = null;

  function notify(error) {
    var box = $("notice");
    box.textContent = error && error.message ? error.message : String(error);
    box.hidden = false;
    clearTimeout(noticeTimer);
    noticeTimer = setTimeout(function () {
      box.hidden = true;
    }, 10000);
  }

  // Runs one user action. The button is disabled while it is in flight, and
  // any error is shown in the notice bar.
  function act(button, action) {
    if (button) {
      button.disabled = true;
    }
    return Promise.resolve()
      .then(action)
      .catch(notify)
      .then(function () {
        if (button) {
          button.disabled = false;
        }
        applyEnabled();
      });
  }

  function setLive(text, tone) {
    var node = $("live");
    node.textContent = text;
    node.className = "muted " + (tone || "");
  }

  // The job in progress. A paused job that has lifted the tool is released
  // (the machine is free to jog), so it is no longer the machine's active job
  // but it is still the one in progress.
  function jobInProgress() {
    var m = model.machine;
    if (m && m.active_job) {
      return m.active_job;
    }
    for (var i = 0; i < model.jobs.length; i++) {
      if (ACTIVE_STATES.indexOf(model.jobs[i].state) >= 0) {
        return model.jobs[i];
      }
    }
    return null;
  }

  // The tab says what the machine is doing, so a job can be watched from
  // another tab or window.
  function updateTitle() {
    var m = model.machine;
    var title = BASE_TITLE;
    var job = jobInProgress();
    if (m && m.alarm !== undefined && m.alarm !== null) {
      title = "ALARM " + m.alarm + " - " + BASE_TITLE;
    } else if (job) {
      var percent = job.total_lines ? Math.floor((100 * job.acked_lines) / job.total_lines) : 0;
      title = (job.state === "paused" ? "Paused " : "") + percent + "% " + job.name + " - " + BASE_TITLE;
    }
    document.title = title;
  }

  // ---- Form values ------------------------------------------------------

  // A number from an input, or undefined when the input is blank.
  function number(input, label) {
    var text = input.value.trim();
    if (text === "") {
      return undefined;
    }
    var value = Number(text);
    if (!isFinite(value)) {
      throw new Error(label + " must be a number");
    }
    return value;
  }

  function requiredNumber(input, label) {
    var value = number(input, label);
    if (value === undefined) {
      throw new Error(label + " is required");
    }
    return value;
  }

  // Axis values for a work-zero or work-offset form, for the chosen system.
  // Blank axes are left out.
  function axisBody(prefix) {
    var body = {};
    var any = false;
    AXIS_KEYS.forEach(function (key) {
      var value = number($(prefix + key), key.toUpperCase());
      if (value !== undefined) {
        body[key] = value;
        any = true;
      }
    });
    if (!any) {
      throw new Error("Enter at least one axis");
    }
    if ($("work-system").value) {
      body.system = $("work-system").value;
    }
    return body;
  }

  function readBounds() {
    var bounds = {};
    AXIS_KEYS.forEach(function (key) {
      var min = number($("b" + key + "-min"), key.toUpperCase() + " minimum");
      var max = number($("b" + key + "-max"), key.toUpperCase() + " maximum");
      if (min === undefined && max === undefined) {
        return;
      }
      if (min === undefined || max === undefined) {
        throw new Error(key.toUpperCase() + " needs both a minimum and a maximum, or neither");
      }
      bounds[key] = [min, max];
    });
    return bounds;
  }

  function resumeOptions() {
    var body = {};
    var z = number($("resume-z"), "Retract height");
    if (z !== undefined) {
      body.retract = { z: z };
      var dwell = number($("resume-dwell"), "Spindle dwell");
      if (dwell !== undefined) {
        body.retract.dwell_s = dwell;
      }
    }
    var line = number($("resume-line"), "From file line");
    if (line !== undefined) {
      body.from_line = Math.floor(line);
    }
    if ($("resume-accept-zero").checked) {
      body.accept_moved_zero = true;
    }
    return body;
  }

  // ---- Machine ----------------------------------------------------------

  function formatAxes(values) {
    if (!values) {
      return "-";
    }
    return values
      .slice(0, 3)
      .map(function (v, i) {
        return AXES[i] + " " + v.toFixed(3);
      })
      .join("   ");
  }

  // Fills a row of the position readout, one cell per axis.
  function setAxes(prefix, values) {
    AXIS_KEYS.forEach(function (key, i) {
      var cell = $(prefix + key);
      cell.textContent = values && values[i] !== undefined ? values[i].toFixed(3) : "-";
    });
  }

  // The zero of the active work system, in machine coordinates.
  function activeZero() {
    var m = model.machine || {};
    if (m.work && m.work.offsets && m.work.offsets[m.work.system]) {
      return m.work.offsets[m.work.system];
    }
    return (m.status && m.status.work_offset) || [0, 0, 0];
  }

  var systemsKey = "";

  // The six work systems, with the active one marked. Rebuilt only when they
  // change, so a button is not replaced under the pointer. While the
  // controller is being asked again, `work` is briefly missing; the last
  // values stay on screen rather than flicker away.
  function renderSystems(work) {
    if (!work) {
      return;
    }
    var key = JSON.stringify(work);
    if (key === systemsKey) {
      return;
    }
    systemsKey = key;
    var body = $("systems-body");
    body.textContent = "";
    Object.keys(work.offsets)
      .sort()
      .forEach(function (name) {
        var zero = work.offsets[name];
        var active = name === work.system;
        body.appendChild(
          h(
            "tr",
            { class: active ? "active" : "" },
            h("th", { text: name }),
            h("td", { text: zero[0].toFixed(3) }),
            h("td", { text: zero[1].toFixed(3) }),
            h("td", { text: zero[2].toFixed(3) }),
            h(
              "td",
              {},
              active
                ? h("span", { text: "active" })
                : h("button", { type: "button", "data-system": name, "data-needs": "move", text: "Use" })
            )
          )
        );
      });
  }

  // The sender asks when the controller disagrees with where it last saw the
  // tool, and jobs may wait for the answer.
  function renderPositionCheck(check) {
    $("position-check").hidden = !check;
    if (!check) {
      return;
    }
    $("pc-question").textContent = check.question;
    $("pc-detail").textContent =
      "Recorded " + formatAxes(check.recorded) + ". The controller now reports " +
      formatAxes(check.reported) + ". " +
      (check.adjusted
        ? "The work zeros were moved to keep the tool where the record puts it."
        : "Jobs wait until you answer.");
  }

  function renderMachine(m) {
    model.machine = m;
    var status = m.status;

    var label = "Disconnected";
    var tone = "bad";
    if (m.ready) {
      label = "Connected";
      tone = "ok";
    } else if (m.connection === "connecting") {
      label = "Connecting";
      tone = "warn";
    } else if (m.connection === "connected") {
      label = "Identifying";
      tone = "warn";
    }
    var pill = $("conn");
    pill.textContent = label;
    pill.className = "pill " + tone;

    $("port").textContent = m.port || "";
    var firmware = "";
    if (m.firmware) {
      firmware = (FIRMWARE_NAMES[m.firmware.kind] || m.firmware.kind) + " " + m.firmware.version;
      if (m.rx_buffer_size) {
        firmware += ", " + m.rx_buffer_size + " byte RX buffer";
      }
    }
    $("firmware").textContent = firmware;

    var chip = $("state");
    chip.textContent = status ? status.state : "-";
    chip.setAttribute("data-state", status ? status.state : "");
    setAxes("dro-work-", status && status.work_position);
    setAxes("dro-mach-", status && status.machine_position);
    setAxes("dro-off-", status && status.work_offset);
    if (m.work) {
      $("sys-label").textContent = m.work.system;
    }
    $("feed").textContent =
      status && status.feed_rate !== undefined && status.feed_rate !== null && status.feed_rate > 0
        ? status.feed_rate + " mm/min"
        : "";

    setText(
      "alarm",
      m.alarm !== undefined && m.alarm !== null
        ? "Alarm " + m.alarm + (m.alarm_message ? ": " + m.alarm_message : "") +
          ". Clear it with Unlock or Home."
        : ""
    );
    setText("last-error", m.last_error ? "Last error: " + m.last_error : "");
    renderSystems(m.work);
    renderPositionCheck(m.position_check);
    updateTitle();
    applyEnabled();
    schedulePreview();
  }

  function handleMachine(m) {
    var before = model.machine && model.machine.active_job;
    var jobChanged = m.active_job ? upsertJob(m.active_job) : false;
    if (m.active_job && !model.selected) {
      model.selected = m.active_job.id;
      jobChanged = true;
    }
    renderMachine(m);
    if (jobChanged) {
      renderJobs();
      renderJobDetail();
    }
    if (before && !m.active_job) {
      loadJobs().catch(notify);
    }
  }

  // What the machine allows right now.
  function abilities() {
    var m = model.machine || {};
    var state = (m.status && m.status.state) || "unknown";
    var ready = !!m.ready;
    var busy = !!m.active_job;
    var controllable = ready && !busy;
    return {
      ready: ready,
      busy: busy,
      // Another job may not start while one is in progress, even a paused
      // one that has freed the machine.
      occupied: !!jobInProgress(),
      state: state,
      controllable: controllable,
      canMove: controllable && (state === "Idle" || state === "Jog"),
    };
  }

  // Why a control is unavailable, shown as its tooltip.
  function reasonFor(need, a, job) {
    if (need === "ready" || need === "move") {
      if (!a.ready) {
        return "Not connected to the controller yet";
      }
      if (a.busy) {
        return "Unavailable while a job is active. Pause or stop it first";
      }
      return "The machine is in " + a.state + ", not idle";
    }
    if (!job) {
      return "Select a job first";
    }
    var notReady = "Not connected to the controller yet";
    switch (need) {
      case "start":
        if (!a.ready) {
          return notReady;
        }
        return a.occupied ? "Another job is in progress" : "Only a loaded, completed or stopped job can start";
      case "pause":
      case "hold":
        return job.state === "running" ? notReady : "Only a running job can be paused";
      case "resume":
        if (!a.ready) {
          return notReady;
        }
        return a.occupied ? "Another job is in progress" : "Nothing to resume: the job is " + job.state;
      case "stop":
        return "Only a running or paused job can be stopped";
      case "delete":
        return "A job that is active cannot be deleted";
      default:
        return "";
    }
  }

  // Enables each control whose `data-needs` the machine allows right now. The
  // server still decides; this only keeps the page from offering what it would
  // refuse, and says why.
  function applyEnabled() {
    var a = abilities();
    var job = findJob(model.selected);
    document.querySelectorAll("[data-needs]").forEach(function (node) {
      var need = node.getAttribute("data-needs");
      var allowed;
      if (need === "ready") {
        allowed = a.controllable;
      } else if (need === "move") {
        allowed = a.canMove;
      } else {
        allowed = jobAllowed(need, job, a.ready, a.occupied);
      }
      node.disabled = !allowed;
      if (allowed) {
        node.removeAttribute("title");
      } else {
        node.setAttribute("title", reasonFor(need, a, job));
      }
    });
  }

  // ---- Jogging ----------------------------------------------------------

  function flash(button) {
    button.classList.add("pressed");
    setTimeout(function () {
      button.classList.remove("pressed");
    }, 160);
  }

  // Sends one jog of the chosen step (times `multiplier`) along an axis.
  function sendJog(axis, sign, multiplier, button) {
    return act(button, function () {
      var step = Number($("jog-step").value) * multiplier;
      var feed = Number($("jog-feed").value);
      if (!(feed > 0)) {
        throw new Error("Feed must be above zero");
      }
      var body = { feed: feed };
      body[axis.toLowerCase()] = sign * step;
      return request("POST", "/machine/jog", body);
    });
  }

  function jogButton(axis, sign) {
    return document.querySelector('button[data-axis="' + axis + '"][data-sign="' + sign + '"]');
  }

  function onKey(event) {
    var target = event.target;
    var tag = target && target.tagName;
    if (tag === "INPUT" || tag === "SELECT" || tag === "TEXTAREA" || (target && target.isContentEditable)) {
      return;
    }
    if (event.ctrlKey || event.metaKey || event.altKey) {
      return;
    }
    if (event.key === "Escape") {
      if (abilities().controllable) {
        act($("jog-cancel"), function () {
          return request("POST", "/machine/jog/cancel");
        });
      }
      return;
    }
    var jog = Lib.jogFromKey(event.key, event.shiftKey);
    if (!jog) {
      return;
    }
    // The arrow keys would scroll the page.
    event.preventDefault();
    if (event.repeat || !abilities().canMove) {
      return;
    }
    var button = jogButton(jog.axis, jog.sign);
    if (button) {
      flash(button);
    }
    sendJog(jog.axis, jog.sign, jog.multiplier, null);
  }

  // ---- Jobs -------------------------------------------------------------

  function findJob(id) {
    for (var i = 0; i < model.jobs.length; i++) {
      if (model.jobs[i].id === id) {
        return model.jobs[i];
      }
    }
    return null;
  }

  // Stores a job record. Returns whether it changed what is shown.
  function upsertJob(job) {
    for (var i = 0; i < model.jobs.length; i++) {
      if (model.jobs[i].id === job.id) {
        var changed = JSON.stringify(model.jobs[i]) !== JSON.stringify(job);
        model.jobs[i] = job;
        return changed;
      }
    }
    model.jobs.push(job);
    return true;
  }

  function storeJob(job) {
    upsertJob(job);
    renderAll();
    // A short job can finish between two events, and the stream then never
    // sends its final state. Ask the server what the job is now.
    return loadJobs().catch(notify);
  }

  function removeJob(id) {
    model.jobs = model.jobs.filter(function (job) {
      return job.id !== id;
    });
    delete programs[id];
    if (model.selected === id) {
      model.selected = null;
    }
    renderAll();
  }

  function jobAllowed(action, job, ready, occupied) {
    if (!job) {
      return false;
    }
    var s = job.state;
    switch (action) {
      case "start":
        return ready && !occupied && STARTABLE.indexOf(s) >= 0;
      case "pause":
      case "hold":
        return ready && s === "running";
      case "resume":
        return ready && (s === "paused" || (!occupied && RESUMABLE_AFTER_STOP.indexOf(s) >= 0));
      case "stop":
        return s === "running" || s === "paused";
      case "delete":
        return DELETABLE.indexOf(s) >= 0;
      default:
        return false;
    }
  }

  function renderJobs() {
    var list = $("job-list");
    list.textContent = "";
    if (model.jobs.length === 0) {
      list.appendChild(h("li", { class: "muted", text: "No programs yet. Upload one above." }));
      return;
    }
    // Newest first.
    model.jobs
      .slice()
      .reverse()
      .forEach(function (job) {
        list.appendChild(
          h(
            "li",
            {},
            h(
              "button",
              {
                type: "button",
                class: "job-row",
                "data-job": job.id,
                "aria-pressed": job.id === model.selected ? "true" : "false",
              },
              h("span", { class: "job-name", text: job.name }),
              h("span", { class: "badge", "data-state": job.state, text: job.state }),
              h("progress", { max: Math.max(job.total_lines, 1), value: job.acked_lines }),
              h("span", { class: "muted", text: job.acked_lines + " / " + job.total_lines + " lines" })
            )
          )
        );
      });
  }

  function renderJobTimes() {
    var job = findJob(model.selected);
    if (!job) {
      return;
    }
    var now = Date.now();
    var parts = [];
    var elapsed = Lib.elapsedMs(job, now);
    if (elapsed !== null) {
      parts.push("Elapsed " + Lib.formatDuration(elapsed));
    }
    var left = Lib.remainingMs(job, now);
    if (left !== null) {
      parts.push("about " + Lib.formatDuration(left) + " left");
    }
    $("job-times").textContent = parts.join(", ");
  }

  function renderJobDetail() {
    var job = findJob(model.selected);
    $("job-detail").hidden = !job;
    if (!job) {
      return;
    }

    $("job-title").textContent = job.name;
    var badge = $("job-state");
    badge.textContent = job.state;
    badge.setAttribute("data-state", job.state);

    var progress = $("job-progress");
    progress.max = Math.max(job.total_lines, 1);
    progress.value = job.acked_lines;

    var counts = job.acked_lines + " of " + job.total_lines + " lines acknowledged";
    if (job.last_acked_line !== null && job.last_acked_line !== undefined) {
      counts += ", last at file line " + job.last_acked_line;
    }
    $("job-counts").textContent = counts;
    renderJobTimes();

    setText("job-reason", job.reason);
    $("job-reason").className = "reason" + (job.state === "failed" || job.state === "interrupted" ? " bad" : "");
    $("job-hint").textContent = Lib.jobHint(job, model.machine);

    var facts = $("job-facts");
    facts.textContent = "";
    if (job.paused_at) {
      var lifted = job.paused_at.lifted_to;
      facts.appendChild(
        h("li", {
          text:
            "Paused at " + formatAxes(job.paused_at.position) +
            (lifted !== null && lifted !== undefined
              ? ", tool lifted to Z " + lifted
              : ", tool not lifted"),
        })
      );
    }
    if (job.work_offsets && job.work_offsets.offsets) {
      var ran = job.work_offsets.offsets[job.work_offsets.system];
      facts.appendChild(
        h("li", {
          text: "Ran in " + job.work_offsets.system + (ran ? ", zero at " + formatAxes(ran) : ""),
        })
      );
    }
    if (job.resumed_from_line !== null && job.resumed_from_line !== undefined) {
      facts.appendChild(h("li", { text: "Last resumed from file line " + job.resumed_from_line }));
    }
    job.line_errors.slice(-5).forEach(function (e) {
      facts.appendChild(
        h("li", {
          class: "error-line",
          text:
            "Line " + e.line + ": error " + e.code +
            (e.message ? " (" + e.message + ")" : "") + ", \"" + e.text + "\"",
        })
      );
    });

    loadProgram(job.id);
    renderPreviewNote();
    schedulePreview();
    applyEnabled();
  }

  function renderAll() {
    renderJobs();
    renderJobDetail();
    applyEnabled();
  }

  function loadJobs() {
    return request("GET", "/jobs").then(function (jobs) {
      model.jobs = jobs;
      if (!findJob(model.selected)) {
        model.selected = null;
      }
      if (!model.selected && model.machine && model.machine.active_job) {
        model.selected = model.machine.active_job.id;
      }
      renderAll();
    });
  }

  function runJobAction(button, action, id) {
    var base = "/jobs/" + encodeURIComponent(id);
    act(button, function () {
      switch (action) {
        case "start":
          return request("POST", base + "/start").then(storeJob);
        case "pause":
          return request("POST", base + "/pause").then(storeJob);
        case "hold":
          return request("POST", base + "/pause", { lift: false }).then(storeJob);
        case "resume":
          return request("POST", base + "/resume", resumeOptions()).then(storeJob);
        case "stop":
          return request("POST", base + "/stop").then(storeJob);
        case "delete":
          if (!window.confirm("Delete this program and its record?")) {
            return null;
          }
          return request("DELETE", base).then(function () {
            removeJob(id);
          });
        default:
          return null;
      }
    });
  }

  // ---- Toolpath preview -------------------------------------------------

  var programs = {}; // job id -> {state: "loading" | "ready" | "error", parsed}
  var previewQueued = false;

  // Fetches and parses a job's program once.
  function loadProgram(id) {
    if (programs[id]) {
      return;
    }
    programs[id] = { state: "loading" };
    fetch("/jobs/" + encodeURIComponent(id) + "/program")
      .then(function (res) {
        if (!res.ok) {
          throw new Error("HTTP " + res.status);
        }
        return res.text();
      })
      .then(function (text) {
        programs[id] = { state: "ready", parsed: Lib.parseProgram(text) };
      })
      .catch(function () {
        programs[id] = { state: "error" };
      })
      .then(function () {
        renderPreviewNote();
        schedulePreview();
      });
  }

  function renderPreviewNote() {
    var job = findJob(model.selected);
    var note = $("preview-note");
    var entry = job && programs[job.id];
    if (!entry || entry.state !== "ready" || !entry.parsed.segments.length) {
      note.textContent = "";
      return;
    }
    var p = entry.parsed;
    // The size of the part is where it cuts; rapids that travel to and from
    // the work do not count. Programs that never cut are sized by all moves.
    var lo = p.cutMin || p.min;
    var hi = p.cutMax || p.max;
    var size =
      (hi[0] - lo[0]).toFixed(1) + " by " + (hi[1] - lo[1]).toFixed(1) +
      " mm in XY, Z " + lo[2].toFixed(1) + " to " + hi[2].toFixed(1);
    var parts = ["Preview, in work coordinates: " + (p.cutMin ? "it cuts " : "it moves ") + size + "."];
    var outside = Lib.outsideBounds(p, model.bounds, activeZero());
    if (outside.length) {
      parts.push(
        "It looks like this leaves the boundaries: " + outside.join("; ") +
        ". The sender checks the real path when you start."
      );
    }
    p.notes.forEach(function (n) {
      parts.push(n + ".");
    });
    if (p.systems.length) {
      parts.push("It selects " + p.systems.join(", ") + "; the preview draws it all in one frame.");
    }
    note.textContent = parts.join(" ");
    $("preview").setAttribute("aria-label", "Toolpath preview, " + size);
  }

  function schedulePreview() {
    if (previewQueued) {
      return;
    }
    previewQueued = true;
    window.requestAnimationFrame(function () {
      previewQueued = false;
      drawPreview();
    });
  }

  function palette() {
    var css = window.getComputedStyle(document.documentElement);
    function color(name) {
      return css.getPropertyValue(name).trim();
    }
    return {
      accent: color("--accent"),
      line: color("--line"),
      muted: color("--muted"),
      warn: color("--warn"),
      card: color("--card"),
    };
  }

  // Draws the program top-down: the path done so far in the accent colour, the
  // rest in grey, rapids dotted, the boundaries dashed, and the tool itself.
  function drawPreview() {
    var job = findJob(model.selected);
    var canvas = $("preview");
    if (!job || $("job-detail").hidden) {
      return;
    }
    var box = canvas.getBoundingClientRect();
    if (box.width === 0) {
      return;
    }
    var ratio = window.devicePixelRatio || 1;
    var width = Math.round(box.width * ratio);
    var height = Math.round(box.height * ratio);
    if (canvas.width !== width || canvas.height !== height) {
      canvas.width = width;
      canvas.height = height;
    }
    var ctx = canvas.getContext("2d");
    ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
    ctx.clearRect(0, 0, box.width, box.height);
    var c = palette();

    var entry = programs[job.id];
    var p = entry && entry.state === "ready" ? entry.parsed : null;
    if (!p || !p.segments.length) {
      ctx.font = "13px system-ui, sans-serif";
      ctx.fillStyle = c.muted;
      ctx.textAlign = "center";
      var message = !entry || entry.state === "loading" ? "Loading preview" : entry.state === "error"
        ? "Preview unavailable"
        : "No moves to draw";
      ctx.fillText(message, box.width / 2, box.height / 2);
      return;
    }

    // Fit the path, and the tool if it is elsewhere.
    var status = (model.machine && model.machine.status) || {};
    var tool = status.work_position;
    var lo = [p.min[0], p.min[1]];
    var hi = [p.max[0], p.max[1]];
    if (tool) {
      lo = [Math.min(lo[0], tool[0]), Math.min(lo[1], tool[1])];
      hi = [Math.max(hi[0], tool[0]), Math.max(hi[1], tool[1])];
    }
    var pad = 16;
    var spanX = Math.max(hi[0] - lo[0], 1e-6);
    var spanY = Math.max(hi[1] - lo[1], 1e-6);
    var scale = Math.min((box.width - 2 * pad) / spanX, (box.height - 2 * pad) / spanY);
    var offX = (box.width - spanX * scale) / 2;
    var offY = (box.height - spanY * scale) / 2;
    function sx(x) {
      return offX + (x - lo[0]) * scale;
    }
    function sy(y) {
      return box.height - offY - (y - lo[1]) * scale;
    }

    // The boundaries, in work coordinates.
    var b = model.bounds;
    if (b && b.x && b.y) {
      var zero = activeZero();
      ctx.save();
      ctx.setLineDash([6, 4]);
      ctx.strokeStyle = c.warn;
      ctx.lineWidth = 1.5;
      ctx.strokeRect(
        sx(b.x[0] - zero[0]),
        sy(b.y[1] - zero[1]),
        (b.x[1] - b.x[0]) * scale,
        (b.y[1] - b.y[0]) * scale
      );
      ctx.restore();
    }

    // The work origin.
    ctx.strokeStyle = c.line;
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(sx(0) - 6, sy(0));
    ctx.lineTo(sx(0) + 6, sy(0));
    ctx.moveTo(sx(0), sy(0) - 6);
    ctx.lineTo(sx(0), sy(0) + 6);
    ctx.stroke();

    var done = job.last_acked_line === null || job.last_acked_line === undefined ? 0 : job.last_acked_line;
    var rapid = [];
    var pending = [];
    var finished = [];
    p.segments.forEach(function (s) {
      (s.rapid ? rapid : s.line <= done ? finished : pending).push(s);
    });
    function stroke(segments, color, widthPx, dash) {
      if (!segments.length) {
        return;
      }
      ctx.save();
      ctx.setLineDash(dash);
      ctx.strokeStyle = color;
      ctx.lineWidth = widthPx;
      ctx.lineJoin = "round";
      ctx.beginPath();
      segments.forEach(function (s) {
        ctx.moveTo(sx(s.a[0]), sy(s.a[1]));
        ctx.lineTo(sx(s.b[0]), sy(s.b[1]));
      });
      ctx.stroke();
      ctx.restore();
    }
    stroke(rapid, c.line, 1, [2, 3]);
    stroke(pending, c.muted, 1.5, []);
    stroke(finished, c.accent, 2, []);

    if (tool) {
      ctx.beginPath();
      ctx.arc(sx(tool[0]), sy(tool[1]), 5, 0, 2 * Math.PI);
      ctx.fillStyle = c.accent;
      ctx.fill();
      ctx.lineWidth = 2;
      ctx.strokeStyle = c.card;
      ctx.stroke();
    }
  }

  // ---- Settings and boundaries -----------------------------------------

  function fillSettings(settings) {
    model.settings = settings;
    var pause = settings.pause;
    $("set-lift").checked = pause.lift;
    $("set-height").value = pause.height;
    $("set-z").value = pause.z === null || pause.z === undefined ? "" : pause.z;
    $("set-dwell").value = pause.dwell_s;
    $("set-trust").checked = !settings.position || settings.position.trust_record !== false;
  }

  function fillBounds(bounds) {
    model.bounds = bounds;
    AXIS_KEYS.forEach(function (key) {
      var range = bounds[key];
      $("b" + key + "-min").value = range ? range[0] : "";
      $("b" + key + "-max").value = range ? range[1] : "";
    });
    renderPreviewNote();
    schedulePreview();
  }

  // ---- Event stream -----------------------------------------------------

  function connectEvents() {
    var events = new EventSource("/events");
    events.onopen = function () {
      setLive("Live", "ok");
      // The stream starts with the current state. Refresh the program list
      // too, in case another client changed it while the stream was down.
      loadJobs().catch(notify);
    };
    events.onerror = function () {
      setLive("Reconnecting", "warn");
    };
    events.addEventListener("machine", function (e) {
      handleMachine(JSON.parse(e.data));
    });
    events.addEventListener("job", function (e) {
      var job = JSON.parse(e.data);
      if (upsertJob(job)) {
        renderJobs();
        renderJobDetail();
      }
      updateTitle();
      applyEnabled();
    });
  }

  // ---- Wiring -----------------------------------------------------------

  function firstSubmit(form) {
    return form.querySelector('button[type="submit"]');
  }

  function init() {
    // What was chosen last time.
    var step = remembered("jog-step", null);
    if (step !== null && $("jog-step").querySelector('option[value="' + step + '"]')) {
      $("jog-step").value = step;
    }
    var feed = Number(remembered("jog-feed", ""));
    if (feed > 0) {
      $("jog-feed").value = feed;
    }
    $("jog-step").addEventListener("change", function () {
      remember("jog-step", $("jog-step").value);
    });
    $("jog-feed").addEventListener("change", function () {
      remember("jog-feed", $("jog-feed").value);
    });

    document.querySelectorAll("[data-action]").forEach(function (button) {
      button.addEventListener("click", function () {
        var path = "/machine/" + button.getAttribute("data-action");
        act(button, function () {
          return request("POST", path);
        });
      });
    });

    document.querySelectorAll("[data-axis]").forEach(function (button) {
      button.addEventListener("click", function (event) {
        sendJog(
          button.getAttribute("data-axis"),
          Number(button.getAttribute("data-sign")),
          event.shiftKey ? 10 : 1,
          button
        );
      });
    });
    document.addEventListener("keydown", onKey);

    $("jog-cancel").addEventListener("click", function () {
      act($("jog-cancel"), function () {
        return request("POST", "/machine/jog/cancel");
      });
    });

    $("position-check").addEventListener("click", function (event) {
      var button = event.target.closest("button[data-moved]");
      if (!button) {
        return;
      }
      act(button, function () {
        return request("POST", "/machine/position-check", {
          moved: button.getAttribute("data-moved") === "true",
        });
      });
    });

    $("systems-body").addEventListener("click", function (event) {
      var button = event.target.closest("button[data-system]");
      if (!button) {
        return;
      }
      act(button, function () {
        return request("PUT", "/machine/work-system", { system: button.getAttribute("data-system") });
      });
    });

    $("zero-form").addEventListener("submit", function (event) {
      event.preventDefault();
      act(firstSubmit(event.target), function () {
        return request("POST", "/machine/work-zero", axisBody("zero-"));
      });
    });

    $("offset-form").addEventListener("submit", function (event) {
      event.preventDefault();
      act(firstSubmit(event.target), function () {
        return request("PUT", "/machine/work-offset", axisBody("offset-"));
      });
    });

    $("upload-form").addEventListener("submit", function (event) {
      event.preventDefault();
      var input = $("file");
      var file = input.files && input.files[0];
      if (!file) {
        notify(new Error("Choose a G-code file first"));
        return;
      }
      act($("upload-button"), function () {
        return readFile(file)
          .then(function (text) {
            return request("POST", "/jobs?name=" + encodeURIComponent(file.name), text);
          })
          .then(function (job) {
            input.value = "";
            model.selected = job.id;
            return storeJob(job);
          });
      });
    });

    $("job-list").addEventListener("click", function (event) {
      var row = event.target.closest("[data-job]");
      if (!row) {
        return;
      }
      model.selected = row.getAttribute("data-job");
      renderAll();
    });

    $("job-detail").addEventListener("click", function (event) {
      var button = event.target.closest("button[data-job-action]");
      if (!button || !model.selected) {
        return;
      }
      runJobAction(button, button.getAttribute("data-job-action"), model.selected);
    });

    $("settings-form").addEventListener("submit", function (event) {
      event.preventDefault();
      act(firstSubmit(event.target), function () {
        var pause = {
          lift: $("set-lift").checked,
          height: requiredNumber($("set-height"), "Lift height"),
          dwell_s: requiredNumber($("set-dwell"), "Spindle dwell"),
        };
        var z = number($("set-z"), "Lift to Z");
        if (z !== undefined) {
          pause.z = z;
        }
        // The server replaces the whole document, so start from what it sent
        // and change only what this form shows.
        var next = JSON.parse(JSON.stringify(model.settings || {}));
        next.pause = pause;
        next.position = { trust_record: $("set-trust").checked };
        return request("PUT", "/settings", next).then(fillSettings);
      });
    });

    $("bounds-form").addEventListener("submit", function (event) {
      event.preventDefault();
      act(firstSubmit(event.target), function () {
        return request("PUT", "/machine/bounds", readBounds()).then(fillBounds);
      });
    });

    $("bounds-clear").addEventListener("click", function () {
      act($("bounds-clear"), function () {
        return request("DELETE", "/machine/bounds").then(function () {
          fillBounds({});
        });
      });
    });

    window.addEventListener("resize", schedulePreview);
    try {
      window.matchMedia("(prefers-color-scheme: dark)").addEventListener("change", schedulePreview);
    } catch (e) {
      // Older browsers: the preview picks up a new scheme on its next redraw.
    }
    setInterval(renderJobTimes, 1000);

    request("GET", "/settings").then(fillSettings).catch(notify);
    request("GET", "/machine/bounds").then(fillBounds).catch(notify);
    request("GET", "/machine").then(handleMachine).catch(notify);
    renderAll();
    connectEvents();
  }

  init();
})();
