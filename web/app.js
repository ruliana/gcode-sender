// The G-code Sender web UI. It is one page over the HTTP API that serves it,
// and it talks to nothing else. The page is an Alpine component (`sender`):
// this file keeps the state and the actions, and index.html says how they
// look. Alpine is its CSP build, so the expressions in index.html stay simple:
// anything with logic in it is a getter or a method here. The logic that needs
// no DOM (the toolpath parser, time estimates, guidance) is in lib.js, which
// has its own tests, and the toolpath viewport is in preview.js.
(function () {
  "use strict";

  var Lib = window.SenderLib;
  var AXIS_NAMES = ["X", "Y", "Z"];
  var AXIS_KEYS = ["x", "y", "z"];
  var FIRMWARE_NAMES = { grbl: "Grbl", grblhal: "grblHAL" };
  var STARTABLE = ["loaded", "completed", "stopped"];
  var DELETABLE = ["loaded", "completed", "stopped", "failed", "interrupted"];
  var RESUMABLE_AFTER_STOP = ["interrupted", "stopped", "failed"];
  var ACTIVE_STATES = ["running", "paused", "stopping"];
  var STEPS = ["0.1", "1", "10", "50"];
  var THEMES = ["auto", "dark", "light"];
  var BASE_TITLE = "G-code Sender";
  var GAUGE_START = -0.75 * Math.PI;
  var GAUGE_SWEEP = 1.5 * Math.PI;

  // ---- Browser storage --------------------------------------------------

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

  function applyTheme(theme) {
    if (theme === "auto") {
      document.documentElement.removeAttribute("data-theme");
    } else {
      document.documentElement.setAttribute("data-theme", theme);
    }
  }

  // Before the page is drawn, so it does not flash the other theme.
  applyTheme(remembered("theme", "auto"));

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

  // ---- Form values ------------------------------------------------------

  // A number from a form field, or undefined when the field is blank.
  function number(text, label) {
    text = text === null || text === undefined ? "" : String(text).trim();
    if (text === "") {
      return undefined;
    }
    var value = Number(text);
    if (!isFinite(value)) {
      throw new Error(label + " must be a number");
    }
    return value;
  }

  function requiredNumber(text, label) {
    var value = number(text, label);
    if (value === undefined) {
      throw new Error(label + " is required");
    }
    return value;
  }

  function blankIfMissing(value) {
    return value === null || value === undefined ? "" : String(value);
  }

  function formatAxes(values) {
    if (!values) {
      return "-";
    }
    return values
      .slice(0, 3)
      .map(function (v, i) {
        return AXIS_NAMES[i] + " " + v.toFixed(3);
      })
      .join("   ");
  }

  function present(value) {
    return value !== null && value !== undefined;
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

  // ---- Outside the component ---------------------------------------------

  // Parsed programs are kept out of Alpine's reactive state: a program can
  // have hundreds of thousands of segments, and nothing needs to watch them.
  // `programVersion` in the component changes when this does.
  var programs = {}; // job id -> {state: "loading" | "ready" | "error", parsed}
  var preview = null; // the toolpath viewport (preview.js)
  var noticeTimer = null;
  var pressTimer = null;
  var droppedFile = null; // a file dropped on the upload box

  function sender() {
    return {
      // What the server last said.
      machine: null,
      work: null, // the last work systems seen; kept while they are re-read
      jobs: [], // job records, oldest first
      selected: null, // id of the job shown in the job panel
      settings: null, // the last settings document the server sent
      bounds: null, // the last boundaries the server sent

      // The page.
      live: "connecting",
      notice: "",
      busy: {}, // actions in flight, by name
      now: Date.now(),
      view: remembered("view", "machine"), // the section shown on a phone
      setupTab: remembered("setup-tab", "zero"),
      theme: THEMES.indexOf(remembered("theme", "auto")) >= 0 ? remembered("theme", "auto") : "auto",
      programVersion: 0,
      previewZoomed: false,
      pressed: "", // the jog button lit by a key press
      dropping: false,
      fileName: "",

      // Forms.
      axes: AXIS_NAMES.map(function (name, i) {
        return { name: name, key: AXIS_KEYS[i], index: i };
      }),
      steps: STEPS,
      jogStep: "1",
      jogFeed: "1000",
      zeroSystem: "",
      zeroForm: { x: "", y: "", z: "" },
      offsetForm: { x: "", y: "", z: "" },
      resumeForm: { z: "", dwell: "", line: "", acceptZero: false },
      settingsForm: { lift: true, height: "", z: "", dwell: "", trust: true },
      boundsForm: { xmin: "", xmax: "", ymin: "", ymax: "", zmin: "", zmax: "" },

      init: function () {
        var self = this;
        var step = remembered("jog-step", null);
        if (STEPS.indexOf(step) >= 0) {
          this.jogStep = step;
        }
        var feed = Number(remembered("jog-feed", ""));
        if (feed > 0) {
          this.jogFeed = String(feed);
        }

        preview = window.SenderPreview.create(this.$refs.viewport, function (zoomed) {
          self.previewZoomed = zoomed;
        });
        this.drawGaugeTicks();

        document.addEventListener("keydown", function (event) {
          self.onKey(event);
        });
        window.addEventListener("resize", function () {
          self.drawPreview();
        });
        try {
          window.matchMedia("(prefers-color-scheme: dark)").addEventListener("change", function () {
            self.drawPreview();
          });
        } catch (e) {
          // Older browsers: the preview picks up a new scheme on its next redraw.
        }
        setInterval(function () {
          self.now = Date.now();
        }, 1000);

        // The tab says what the machine is doing, so a job can be watched from
        // another tab or window.
        window.Alpine.effect(function () {
          document.title = self.pageTitle;
        });

        request("GET", "/settings").then(function (s) {
          self.fillSettings(s);
        }).catch(function (e) {
          self.notify(e);
        });
        request("GET", "/machine/bounds").then(function (b) {
          self.fillBounds(b);
        }).catch(function (e) {
          self.notify(e);
        });
        request("GET", "/machine").then(function (m) {
          self.handleMachine(m);
        }).catch(function (e) {
          self.notify(e);
        });
        this.connectEvents();
        this.drawPreview();
      },

      // ---- Feedback -----------------------------------------------------

      notify: function (error) {
        var self = this;
        this.notice = error && error.message ? error.message : String(error);
        clearTimeout(noticeTimer);
        noticeTimer = setTimeout(function () {
          self.notice = "";
        }, 10000);
      },

      // Runs one user action. Its controls are disabled while it is in
      // flight, and any error is shown in the notice.
      act: function (name, action) {
        var self = this;
        this.busy[name] = true;
        return Promise.resolve()
          .then(action)
          .catch(function (e) {
            self.notify(e);
          })
          .then(function () {
            self.busy[name] = false;
          });
      },

      isBusy: function (name) {
        return !!this.busy[name];
      },

      // ---- Header -------------------------------------------------------

      get status() {
        return (this.machine && this.machine.status) || null;
      },

      get connText() {
        var m = this.machine;
        if (!m) {
          return "Starting";
        }
        if (m.ready) {
          return "Connected";
        }
        if (m.connection === "connecting") {
          return "Connecting";
        }
        if (m.connection === "connected") {
          return "Identifying";
        }
        return "Disconnected";
      },

      get connTone() {
        var m = this.machine;
        if (m && m.ready) {
          return "ok";
        }
        return m && (m.connection === "connecting" || m.connection === "connected") ? "warn" : "bad";
      },

      get liveText() {
        return { connecting: "Connecting to events", live: "Live", reconnecting: "Reconnecting" }[this.live];
      },

      get liveTone() {
        return { connecting: "warn", live: "ok", reconnecting: "warn" }[this.live];
      },

      get firmwareText() {
        var m = this.machine;
        if (!m || !m.firmware) {
          return "No controller identified";
        }
        var text = (FIRMWARE_NAMES[m.firmware.kind] || m.firmware.kind) + " " + m.firmware.version;
        if (m.rx_buffer_size) {
          text += ", " + m.rx_buffer_size + " byte RX buffer";
        }
        return text;
      },

      get portText() {
        return this.machine && this.machine.port ? " on " + this.machine.port : "";
      },

      get stateText() {
        if (!this.machine || !this.machine.ready) {
          return this.connText;
        }
        return this.status ? this.status.state : "-";
      },

      // The stack light: red for an alarm or no controller, amber when the
      // machine waits on someone (a hold, a paused job, a question), green
      // when it is fine.
      get lamp() {
        var m = this.machine;
        if (!m || !m.ready || present(m.alarm)) {
          return "red";
        }
        var state = this.status ? this.status.state : "";
        if (state === "Alarm") {
          return "red";
        }
        var job = this.jobInProgress();
        if (m.position_check || (job && job.state === "paused") || ["Hold", "Door", "Check", "Sleep"].indexOf(state) >= 0) {
          return "amber";
        }
        return "green";
      },

      get lampBlinks() {
        var state = this.status ? this.status.state : "";
        return state === "Run" || state === "Jog" || state === "Home";
      },

      cycleTheme: function () {
        this.theme = THEMES[(THEMES.indexOf(this.theme) + 1) % THEMES.length];
        remember("theme", this.theme);
        applyTheme(this.theme);
        this.drawPreview();
      },

      show: function (view) {
        this.view = view;
        remember("view", view);
        this.drawPreview();
      },

      get dockPercent() {
        var job = this.jobInProgress();
        return job ? this.percentOf(job) + "%" : "";
      },

      get pageTitle() {
        var m = this.machine;
        var job = this.jobInProgress();
        if (m && present(m.alarm)) {
          return "ALARM " + m.alarm + " - " + BASE_TITLE;
        }
        if (job) {
          return (job.state === "paused" ? "Paused " : "") + this.percentOf(job) + "% " + job.name + " - " + BASE_TITLE;
        }
        return BASE_TITLE;
      },

      // ---- Alerts -------------------------------------------------------

      get alarmText() {
        var m = this.machine;
        if (!m || !present(m.alarm)) {
          return "";
        }
        return m.alarm + (m.alarm_message ? ": " + m.alarm_message : "") + ". Clear it with Unlock or Home.";
      },

      get check() {
        return (this.machine && this.machine.position_check) || null;
      },

      get checkDetail() {
        var check = this.check;
        if (!check) {
          return "";
        }
        return (
          "Recorded " + formatAxes(check.recorded) + ". The controller now reports " +
          formatAxes(check.reported) + ". " +
          (check.adjusted
            ? "The work zeros were moved to keep the tool where the record puts it."
            : "Jobs wait until you answer.")
        );
      },

      answerCheck: function (moved) {
        return this.act("check", function () {
          return request("POST", "/machine/position-check", { moved: moved });
        });
      },

      // ---- Position -----------------------------------------------------

      // One axis of a position: "work", "machine" or "offset" (the zero).
      axisValues: function (kind) {
        var s = this.status;
        if (!s) {
          return null;
        }
        return kind === "work" ? s.work_position : kind === "machine" ? s.machine_position : s.work_offset;
      },

      axisValue: function (kind, i) {
        var values = this.axisValues(kind);
        return values && present(values[i]) ? values[i].toFixed(3) : "-";
      },

      // The work position for the seven-segment readout: right-aligned under
      // its unlit "88888.888", with "!" (all segments off) as padding.
      digits: function (kind, i) {
        var values = this.axisValues(kind);
        if (!values || !present(values[i])) {
          return "!!!!-.---";
        }
        var text = values[i].toFixed(3);
        if (text.length > 9) {
          return text.slice(0, 9);
        }
        return "!!!!!!!!!".slice(0, 9 - text.length) + text;
      },

      get feedText() {
        var s = this.status;
        return s && present(s.feed_rate) ? s.feed_rate + " mm/min" : "-";
      },

      get spindleText() {
        var s = this.status;
        return s && present(s.spindle_speed) ? s.spindle_speed + " rpm" : "-";
      },

      get lastErrorText() {
        return this.machine && this.machine.last_error ? "Last error: " + this.machine.last_error : "";
      },

      machineAction: function (action) {
        return this.act(action, function () {
          return request("POST", "/machine/" + action);
        });
      },

      handleMachine: function (m) {
        var before = this.machine && this.machine.active_job;
        if (m.active_job) {
          this.upsertJob(m.active_job);
          if (!this.selected) {
            this.selected = m.active_job.id;
          }
        }
        this.machine = m;
        if (m.work) {
          // While the controller is being asked again, `work` is briefly
          // missing; the last values stay on screen rather than flicker away.
          this.work = m.work;
        }
        if (before && !m.active_job) {
          this.loadJobs();
        }
        this.drawPreview();
      },

      // ---- What the machine allows ------------------------------------

      // The job in progress. A paused job that has lifted the tool is
      // released (the machine is free to jog), so it is no longer the
      // machine's active job but it is still the one in progress.
      jobInProgress: function () {
        var m = this.machine;
        if (m && m.active_job) {
          return m.active_job;
        }
        for (var i = 0; i < this.jobs.length; i++) {
          if (ACTIVE_STATES.indexOf(this.jobs[i].state) >= 0) {
            return this.jobs[i];
          }
        }
        return null;
      },

      abilities: function () {
        var m = this.machine || {};
        var state = (m.status && m.status.state) || "unknown";
        var ready = !!m.ready;
        var busy = !!m.active_job;
        var controllable = ready && !busy;
        return {
          ready: ready,
          busy: busy,
          // Another job may not start while one is in progress, even a
          // paused one that has freed the machine.
          occupied: !!this.jobInProgress(),
          state: state,
          controllable: controllable,
          canMove: controllable && (state === "Idle" || state === "Jog"),
        };
      },

      // Whether the machine allows what a control needs: "ready" (idle
      // controller, no job), "move" (and not moving), or a job action. The
      // server still decides; this only keeps the page from offering what it
      // would refuse.
      allowed: function (need) {
        var a = this.abilities();
        if (need === "ready") {
          return a.controllable;
        }
        if (need === "move") {
          return a.canMove;
        }
        return jobAllowed(need, this.job, a.ready, a.occupied);
      },

      // Whether a control is off: not allowed, or its action is in flight.
      off: function (need, action) {
        return !!(action && this.busy[action]) || !this.allowed(need);
      },

      // Why a control is unavailable, shown as its tooltip.
      why: function (need) {
        if (this.allowed(need)) {
          return "";
        }
        var a = this.abilities();
        var job = this.job;
        var notReady = "Not connected to the controller yet";
        if (need === "ready" || need === "move") {
          if (!a.ready) {
            return notReady;
          }
          if (a.busy) {
            return "Unavailable while a job is active. Pause or stop it first";
          }
          return "The machine is in " + a.state + ", not idle";
        }
        if (!job) {
          return "Select a job first";
        }
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
      },

      // ---- Jogging ------------------------------------------------------

      rememberJog: function () {
        remember("jog-step", this.jogStep);
        remember("jog-feed", this.jogFeed);
      },

      // Sends one jog of the chosen step (times `multiplier`) along an axis.
      sendJog: function (axis, sign, multiplier) {
        var self = this;
        return this.act("jog", function () {
          var step = Number(self.jogStep) * multiplier;
          var feed = Number(self.jogFeed);
          if (!(feed > 0)) {
            throw new Error("Feed must be above zero");
          }
          var body = { feed: feed };
          body[axis.toLowerCase()] = sign * step;
          return request("POST", "/machine/jog", body);
        });
      },

      jogClick: function (axis, sign, event) {
        this.sendJog(axis, sign, event && event.shiftKey ? 10 : 1);
      },

      cancelJog: function () {
        return this.act("cancel", function () {
          return request("POST", "/machine/jog/cancel");
        });
      },

      // A key press lights its button, so the jog is seen to have been sent.
      flash: function (name) {
        var self = this;
        this.pressed = name;
        clearTimeout(pressTimer);
        pressTimer = setTimeout(function () {
          self.pressed = "";
        }, 160);
      },

      onKey: function (event) {
        var target = event.target;
        var tag = target && target.tagName;
        if (tag === "INPUT" || tag === "SELECT" || tag === "TEXTAREA" || (target && target.isContentEditable)) {
          return;
        }
        if (event.ctrlKey || event.metaKey || event.altKey) {
          return;
        }
        if (event.key === "Escape") {
          if (this.abilities().controllable) {
            this.cancelJog();
          }
          return;
        }
        var jog = Lib.jogFromKey(event.key, event.shiftKey);
        if (!jog) {
          return;
        }
        // The arrow keys would scroll the page.
        event.preventDefault();
        if (event.repeat || !this.abilities().canMove) {
          return;
        }
        this.flash(jog.axis + jog.sign);
        this.sendJog(jog.axis, jog.sign, jog.multiplier);
      },

      // ---- Work systems ---------------------------------------------------

      // The six work systems, with the active one marked.
      get systems() {
        var work = this.work;
        if (!work) {
          return [];
        }
        return Object.keys(work.offsets)
          .sort()
          .map(function (name) {
            var zero = work.offsets[name];
            return {
              name: name,
              active: name === work.system,
              x: zero[0].toFixed(3),
              y: zero[1].toFixed(3),
              z: zero[2].toFixed(3),
            };
          });
      },

      // The zero of the active work system, in machine coordinates.
      activeZero: function () {
        var m = this.machine || {};
        var work = this.work;
        if (work && work.offsets && work.offsets[work.system]) {
          return work.offsets[work.system];
        }
        return (m.status && m.status.work_offset) || [0, 0, 0];
      },

      useSystem: function (name) {
        return this.act("system", function () {
          return request("PUT", "/machine/work-system", { system: name });
        });
      },

      // Axis values for a work-zero or work-offset form, for the chosen
      // system. Blank axes are left out.
      axisBody: function (form) {
        var body = {};
        var any = false;
        AXIS_KEYS.forEach(function (key) {
          var value = number(form[key], key.toUpperCase());
          if (value !== undefined) {
            body[key] = value;
            any = true;
          }
        });
        if (!any) {
          throw new Error("Enter at least one axis");
        }
        if (this.zeroSystem) {
          body.system = this.zeroSystem;
        }
        return body;
      },

      setZero: function () {
        var self = this;
        return this.act("zero", function () {
          return request("POST", "/machine/work-zero", self.axisBody(self.zeroForm));
        });
      },

      setOffset: function () {
        var self = this;
        return this.act("offset", function () {
          return request("PUT", "/machine/work-offset", self.axisBody(self.offsetForm));
        });
      },

      // ---- Jobs -----------------------------------------------------------

      get job() {
        return this.findJob(this.selected);
      },

      get jobsNewestFirst() {
        return this.jobs.slice().reverse();
      },

      findJob: function (id) {
        for (var i = 0; i < this.jobs.length; i++) {
          if (this.jobs[i].id === id) {
            return this.jobs[i];
          }
        }
        return null;
      },

      // Stores a job record, replacing it only when it changed.
      upsertJob: function (job) {
        for (var i = 0; i < this.jobs.length; i++) {
          if (this.jobs[i].id === job.id) {
            if (JSON.stringify(this.jobs[i]) !== JSON.stringify(job)) {
              this.jobs[i] = job;
            }
            return;
          }
        }
        this.jobs.push(job);
      },

      storeJob: function (job) {
        this.upsertJob(job);
        this.drawPreview();
        // A short job can finish between two events, and the stream then
        // never sends its final state. Ask the server what the job is now.
        return this.loadJobs();
      },

      removeJob: function (id) {
        this.jobs = this.jobs.filter(function (job) {
          return job.id !== id;
        });
        delete programs[id];
        if (this.selected === id) {
          this.selected = null;
        }
        this.drawPreview();
      },

      loadJobs: function () {
        var self = this;
        return request("GET", "/jobs")
          .then(function (jobs) {
            self.jobs = jobs;
            if (!self.findJob(self.selected)) {
              self.selected = null;
            }
            if (!self.selected && self.machine && self.machine.active_job) {
              self.selected = self.machine.active_job.id;
            }
            // With nothing chosen, show the job in progress or else the
            // newest program.
            if (!self.selected && jobs.length) {
              var current = self.jobInProgress();
              self.selected = current ? current.id : jobs[jobs.length - 1].id;
            }
            self.drawPreview();
          })
          .catch(function (e) {
            self.notify(e);
          });
      },

      select: function (id) {
        this.selected = id;
        this.drawPreview();
      },

      percentOf: function (job) {
        return job.total_lines ? Math.floor((100 * job.acked_lines) / job.total_lines) : 0;
      },

      rowPercent: function (job) {
        return (job.total_lines ? (100 * job.acked_lines) / job.total_lines : 0) + "%";
      },

      get percentWidth() {
        return this.job ? this.rowPercent(this.job) : "0%";
      },

      get percentText() {
        return this.job ? this.percentOf(this.job) + "%" : "";
      },

      get percentDigits() {
        return this.job ? String(this.percentOf(this.job)) : "";
      },

      get linesText() {
        var job = this.job;
        return job ? job.acked_lines + " / " + job.total_lines : "";
      },

      get fileLineText() {
        var job = this.job;
        return job && present(job.last_acked_line) ? String(job.last_acked_line) : "-";
      },

      get elapsedText() {
        var job = this.job;
        var elapsed = job ? Lib.elapsedMs(job, this.now) : null;
        return elapsed === null ? "-" : Lib.formatDuration(elapsed);
      },

      get remainingText() {
        var job = this.job;
        var left = job ? Lib.remainingMs(job, this.now) : null;
        return left === null ? "-" : "about " + Lib.formatDuration(left);
      },

      get hintText() {
        return this.job ? Lib.jobHint(this.job, this.machine) : "";
      },

      get reasonIsBad() {
        var job = this.job;
        return !!job && (job.state === "failed" || job.state === "interrupted");
      },

      // What happened to the selected job, one line each.
      get facts() {
        var job = this.job;
        var facts = [];
        if (!job) {
          return facts;
        }
        if (job.paused_at) {
          var lifted = job.paused_at.lifted_to;
          facts.push({
            key: "paused",
            text:
              "Paused at " + formatAxes(job.paused_at.position) +
              (present(lifted) ? ", tool lifted to Z " + lifted : ", tool not lifted"),
          });
        }
        if (job.work_offsets && job.work_offsets.offsets) {
          var ran = job.work_offsets.offsets[job.work_offsets.system];
          facts.push({
            key: "ran",
            text: "Ran in " + job.work_offsets.system + (ran ? ", zero at " + formatAxes(ran) : ""),
          });
        }
        if (present(job.resumed_from_line)) {
          facts.push({ key: "resumed", text: "Last resumed from file line " + job.resumed_from_line });
        }
        job.line_errors.slice(-5).forEach(function (e, i) {
          facts.push({
            key: "error" + i,
            bad: true,
            text:
              "Line " + e.line + ": error " + e.code +
              (e.message ? " (" + e.message + ")" : "") + ", \"" + e.text + "\"",
          });
        });
        return facts;
      },

      resumeOptions: function () {
        var form = this.resumeForm;
        var body = {};
        var z = number(form.z, "Retract height");
        if (z !== undefined) {
          body.retract = { z: z };
          var dwell = number(form.dwell, "Spindle dwell");
          if (dwell !== undefined) {
            body.retract.dwell_s = dwell;
          }
        }
        var line = number(form.line, "From file line");
        if (line !== undefined) {
          body.from_line = Math.floor(line);
        }
        if (form.acceptZero) {
          body.accept_moved_zero = true;
        }
        return body;
      },

      jobAction: function (action) {
        var self = this;
        var id = this.selected;
        if (!id) {
          return null;
        }
        var base = "/jobs/" + encodeURIComponent(id);
        function store(job) {
          return self.storeJob(job);
        }
        return this.act(action, function () {
          switch (action) {
            case "start":
              return request("POST", base + "/start").then(store);
            case "pause":
              return request("POST", base + "/pause").then(store);
            case "hold":
              return request("POST", base + "/pause", { lift: false }).then(store);
            case "resume":
              return request("POST", base + "/resume", self.resumeOptions()).then(store);
            case "stop":
              return request("POST", base + "/stop").then(store);
            case "delete":
              if (!window.confirm("Delete this program and its record?")) {
                return null;
              }
              return request("DELETE", base).then(function () {
                self.removeJob(id);
              });
            default:
              return null;
          }
        });
      },

      // ---- Upload -------------------------------------------------------

      chosenFile: function () {
        var input = this.$refs.file;
        return (input.files && input.files[0]) || droppedFile;
      },

      fileChosen: function () {
        droppedFile = null;
        var file = this.chosenFile();
        this.fileName = file ? file.name : "";
      },

      dragOver: function () {
        this.dropping = true;
      },

      dragLeave: function () {
        this.dropping = false;
      },

      dropFile: function (event) {
        this.dropping = false;
        var files = event.dataTransfer && event.dataTransfer.files;
        if (!files || !files.length) {
          return;
        }
        this.$refs.file.value = "";
        droppedFile = files[0];
        this.fileName = droppedFile.name;
      },

      upload: function () {
        var self = this;
        var file = this.chosenFile();
        if (!file) {
          this.notify(new Error("Choose a G-code file first"));
          return null;
        }
        return this.act("upload", function () {
          return readFile(file)
            .then(function (text) {
              return request("POST", "/jobs?name=" + encodeURIComponent(file.name), text);
            })
            .then(function (job) {
              self.$refs.file.value = "";
              droppedFile = null;
              self.fileName = "";
              self.selected = job.id;
              return self.storeJob(job);
            });
        });
      },

      // ---- Progress gauge -------------------------------------------------

      // The dial's scale marks, drawn once with d3.
      drawGaugeTicks: function () {
        var ticks = d3.range(0, 101, 5);
        d3.select(this.$refs.gaugeTicks)
          .selectAll("line")
          .data(ticks)
          .join("line")
          .attr("class", function (t) {
            return t % 25 === 0 ? "major" : "minor";
          })
          .attr("x1", function (t) {
            return Math.sin(GAUGE_START + (GAUGE_SWEEP * t) / 100) * 55;
          })
          .attr("y1", function (t) {
            return -Math.cos(GAUGE_START + (GAUGE_SWEEP * t) / 100) * 55;
          })
          .attr("x2", function (t) {
            return Math.sin(GAUGE_START + (GAUGE_SWEEP * t) / 100) * (t % 25 === 0 ? 47 : 51);
          })
          .attr("y2", function (t) {
            return -Math.cos(GAUGE_START + (GAUGE_SWEEP * t) / 100) * (t % 25 === 0 ? 47 : 51);
          });
      },

      gaugeArc: function (fraction) {
        return d3.arc()({
          innerRadius: 37,
          outerRadius: 44,
          startAngle: GAUGE_START,
          endAngle: GAUGE_START + GAUGE_SWEEP * Math.max(0, Math.min(1, fraction)),
          cornerRadius: 1,
        });
      },

      get gaugeTrack() {
        return this.gaugeArc(1);
      },

      get gaugeFill() {
        var job = this.job;
        return this.gaugeArc(job && job.total_lines ? job.acked_lines / job.total_lines : 0);
      },

      // ---- Toolpath preview ---------------------------------------------

      // Fetches and parses a job's program once.
      loadProgram: function (id) {
        var self = this;
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
            self.programVersion++;
            self.drawPreview();
          });
      },

      // The selected job's parsed program, if it is ready.
      program: function () {
        var job = this.job;
        var entry = job && programs[job.id];
        // Read so the getters that use this follow the programs.
        return this.programVersion >= 0 && entry && entry.state === "ready" ? entry.parsed : null;
      },

      get previewReady() {
        var p = this.program();
        return !!p && p.segments.length > 0;
      },

      get previewMessage() {
        var job = this.job;
        if (!job) {
          return this.jobs.length ? "Select a program to see its toolpath" : "Upload a program to see its toolpath";
        }
        var entry = this.programVersion >= 0 && programs[job.id];
        if (!entry || entry.state === "loading") {
          return "Loading preview";
        }
        if (entry.state === "error") {
          return "Preview unavailable";
        }
        return entry.parsed.segments.length ? "" : "No moves to draw";
      },

      // The size of the part: where it cuts. Rapids that travel to and from
      // the work do not count. Programs that never cut are sized by all moves.
      get previewSize() {
        var p = this.program();
        if (!p || !p.segments.length) {
          return "";
        }
        var lo = p.cutMin || p.min;
        var hi = p.cutMax || p.max;
        return (
          (hi[0] - lo[0]).toFixed(1) + " by " + (hi[1] - lo[1]).toFixed(1) +
          " mm in XY, Z " + lo[2].toFixed(1) + " to " + hi[2].toFixed(1)
        );
      },

      get previewLabel() {
        return this.previewSize ? "Toolpath preview, " + this.previewSize : "Toolpath preview";
      },

      get outside() {
        return Lib.outsideBounds(this.program(), this.bounds, this.activeZero());
      },

      get previewWarns() {
        return this.outside.length > 0;
      },

      get previewNote() {
        var p = this.program();
        if (!p || !p.segments.length) {
          return "";
        }
        var parts = ["Work coordinates: " + (p.cutMin ? "it cuts " : "it moves ") + this.previewSize + "."];
        var outside = this.outside;
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
        return parts.join(" ");
      },

      palette: function () {
        var css = window.getComputedStyle(document.documentElement);
        function color(name) {
          return css.getPropertyValue(name).trim();
        }
        return {
          bg: color("--view-bg"),
          grid: color("--view-grid"),
          gridMinor: color("--view-grid-minor"),
          done: color("--path-done"),
          pending: color("--path-pending"),
          rapid: color("--path-rapid"),
          tool: color("--path-tool"),
          hazard: color("--hazard"),
          hazardDark: color("--hazard-dark"),
          axisX: color("--axis-x"),
          axisY: color("--axis-y"),
          text: color("--view-text"),
        };
      },

      // Draws the selected program: the path done so far, the rest, rapids,
      // the boundaries in work coordinates, and the tool.
      drawPreview: function () {
        if (!preview) {
          return;
        }
        var job = this.job;
        if (job) {
          this.loadProgram(job.id);
        }
        var b = this.bounds;
        var zero = this.activeZero();
        var status = this.status || {};
        preview.draw({
          key: job ? job.id : null,
          parsed: this.program(),
          done: job && present(job.last_acked_line) ? job.last_acked_line : 0,
          tool: status.work_position ? [status.work_position[0], status.work_position[1]] : null,
          bounds:
            b && b.x && b.y
              ? { x: [b.x[0] - zero[0], b.x[1] - zero[0]], y: [b.y[0] - zero[1], b.y[1] - zero[1]] }
              : null,
          palette: this.palette(),
        });
      },

      zoomPreview: function (factor) {
        preview.zoomBy(factor);
      },

      fitPreview: function () {
        preview.fit();
      },

      // ---- Settings and boundaries --------------------------------------

      fillSettings: function (settings) {
        this.settings = settings;
        var pause = settings.pause;
        this.settingsForm.lift = !!pause.lift;
        this.settingsForm.height = blankIfMissing(pause.height);
        this.settingsForm.z = blankIfMissing(pause.z);
        this.settingsForm.dwell = blankIfMissing(pause.dwell_s);
        this.settingsForm.trust = !settings.position || settings.position.trust_record !== false;
      },

      saveSettings: function () {
        var self = this;
        return this.act("settings", function () {
          var form = self.settingsForm;
          var pause = {
            lift: !!form.lift,
            height: requiredNumber(form.height, "Lift height"),
            dwell_s: requiredNumber(form.dwell, "Spindle dwell"),
          };
          var z = number(form.z, "Lift to Z");
          if (z !== undefined) {
            pause.z = z;
          }
          // The server replaces the whole document, so start from what it
          // sent and change only what this form shows.
          var next = JSON.parse(JSON.stringify(self.settings || {}));
          next.pause = pause;
          next.position = { trust_record: !!form.trust };
          return request("PUT", "/settings", next).then(function (s) {
            self.fillSettings(s);
          });
        });
      },

      fillBounds: function (bounds) {
        var self = this;
        this.bounds = bounds;
        AXIS_KEYS.forEach(function (key) {
          var range = bounds[key];
          self.boundsForm[key + "min"] = range ? String(range[0]) : "";
          self.boundsForm[key + "max"] = range ? String(range[1]) : "";
        });
        this.drawPreview();
      },

      readBounds: function () {
        var form = this.boundsForm;
        var bounds = {};
        AXIS_KEYS.forEach(function (key) {
          var min = number(form[key + "min"], key.toUpperCase() + " minimum");
          var max = number(form[key + "max"], key.toUpperCase() + " maximum");
          if (min === undefined && max === undefined) {
            return;
          }
          if (min === undefined || max === undefined) {
            throw new Error(key.toUpperCase() + " needs both a minimum and a maximum, or neither");
          }
          bounds[key] = [min, max];
        });
        return bounds;
      },

      saveBounds: function () {
        var self = this;
        return this.act("bounds", function () {
          return request("PUT", "/machine/bounds", self.readBounds()).then(function (b) {
            self.fillBounds(b);
          });
        });
      },

      clearBounds: function () {
        var self = this;
        return this.act("boundsClear", function () {
          return request("DELETE", "/machine/bounds").then(function () {
            self.fillBounds({});
          });
        });
      },

      // ---- Event stream -------------------------------------------------

      connectEvents: function () {
        var self = this;
        var events = new EventSource("/events");
        events.onopen = function () {
          self.live = "live";
          // The stream starts with the current state. Refresh the program
          // list too, in case another client changed it while it was down.
          self.loadJobs();
        };
        events.onerror = function () {
          self.live = "reconnecting";
        };
        events.addEventListener("machine", function (e) {
          self.handleMachine(JSON.parse(e.data));
        });
        events.addEventListener("job", function (e) {
          self.upsertJob(JSON.parse(e.data));
          self.drawPreview();
        });
      },
    };
  }

  document.addEventListener("alpine:init", function () {
    window.Alpine.data("sender", sender);
  });
})();
