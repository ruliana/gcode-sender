// The G-code Sender web UI. It is one page over the HTTP API that serves it,
// and it talks to nothing else. It is a plain script with no build step and
// avoids newer syntax, so it runs in older browsers.
(function () {
  "use strict";

  var AXES = ["X", "Y", "Z"];
  var AXIS_KEYS = ["x", "y", "z"];
  var FIRMWARE_NAMES = { grbl: "Grbl", grblhal: "grblHAL" };
  var STARTABLE = ["loaded", "completed", "stopped"];
  var DELETABLE = ["loaded", "completed", "stopped", "failed", "interrupted"];
  var RESUMABLE_AFTER_STOP = ["interrupted", "stopped", "failed"];

  var model = {
    machine: null,
    jobs: [], // job records, oldest first
    selected: null, // id of the job shown in the detail panel
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

  // Axis values for a work-zero or work-offset form. Blank axes are left out.
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

    $("state").textContent = status ? status.state : "-";
    $("mpos").textContent = status ? formatAxes(status.machine_position) : "-";
    $("wpos").textContent = status ? formatAxes(status.work_position) : "-";
    $("woff").textContent = status ? formatAxes(status.work_offset) : "-";
    $("feed").textContent =
      status && status.feed_rate !== undefined && status.feed_rate !== null
        ? status.feed_rate + " mm/min"
        : "-";

    setText(
      "alarm",
      m.alarm !== undefined && m.alarm !== null
        ? "Alarm " + m.alarm + (m.alarm_message ? ": " + m.alarm_message : "") +
          ". Clear it with Unlock or Home."
        : ""
    );
    setText("last-error", m.last_error ? "Last error: " + m.last_error : "");
    applyEnabled();
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

  // Enables each control whose `data-needs` the machine allows right now. The
  // server still decides; this only keeps the page from offering what it would
  // refuse.
  function applyEnabled() {
    var m = model.machine || {};
    var status = m.status || {};
    var ready = !!m.ready;
    var busy = !!m.active_job;
    var controllable = ready && !busy;
    var canMove = controllable && (status.state === "Idle" || status.state === "Jog");
    var job = findJob(model.selected);

    document.querySelectorAll("[data-needs]").forEach(function (node) {
      var need = node.getAttribute("data-needs");
      var allowed;
      if (need === "ready") {
        allowed = controllable;
      } else if (need === "move") {
        allowed = canMove;
      } else {
        allowed = jobAllowed(need, job, ready, busy);
      }
      node.disabled = !allowed;
    });
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
    if (model.selected === id) {
      model.selected = null;
    }
    renderAll();
  }

  function jobAllowed(action, job, ready, busy) {
    if (!job) {
      return false;
    }
    var s = job.state;
    switch (action) {
      case "start":
        return ready && !busy && STARTABLE.indexOf(s) >= 0;
      case "pause":
      case "hold":
        return ready && s === "running";
      case "resume":
        return ready && (s === "paused" || (!busy && RESUMABLE_AFTER_STOP.indexOf(s) >= 0));
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
    setText("job-reason", job.reason);

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
    if (job.work_offset) {
      facts.appendChild(h("li", { text: "Ran with work offset " + formatAxes(job.work_offset) }));
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

  // ---- Settings and boundaries -----------------------------------------

  function fillSettings(settings) {
    var pause = settings.pause;
    $("set-lift").checked = pause.lift;
    $("set-height").value = pause.height;
    $("set-z").value = pause.z === null || pause.z === undefined ? "" : pause.z;
    $("set-dwell").value = pause.dwell_s;
  }

  function fillBounds(bounds) {
    AXIS_KEYS.forEach(function (key) {
      var range = bounds[key];
      $("b" + key + "-min").value = range ? range[0] : "";
      $("b" + key + "-max").value = range ? range[1] : "";
    });
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
      applyEnabled();
    });
  }

  // ---- Wiring -----------------------------------------------------------

  function firstSubmit(form) {
    return form.querySelector('button[type="submit"]');
  }

  function init() {
    document.querySelectorAll("[data-action]").forEach(function (button) {
      button.addEventListener("click", function () {
        var path = "/machine/" + button.getAttribute("data-action");
        act(button, function () {
          return request("POST", path);
        });
      });
    });

    document.querySelectorAll("[data-axis]").forEach(function (button) {
      button.addEventListener("click", function () {
        act(button, function () {
          var step = Number($("jog-step").value);
          var feed = Number($("jog-feed").value);
          if (!(feed > 0)) {
            throw new Error("Feed must be above zero");
          }
          var body = { feed: feed };
          body[button.getAttribute("data-axis").toLowerCase()] =
            Number(button.getAttribute("data-sign")) * step;
          return request("POST", "/machine/jog", body);
        });
      });
    });

    $("jog-cancel").addEventListener("click", function () {
      act($("jog-cancel"), function () {
        return request("POST", "/machine/jog/cancel");
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
        return request("PUT", "/settings", { pause: pause }).then(fillSettings);
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

    request("GET", "/settings").then(fillSettings).catch(notify);
    request("GET", "/machine/bounds").then(fillBounds).catch(notify);
    request("GET", "/machine").then(handleMachine).catch(notify);
    renderAll();
    connectEvents();
  }

  init();
})();
