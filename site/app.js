(() => {
  'use strict';

  const $ = (sel, root = document) => root.querySelector(sel);
  const $$ = (sel, root = document) => Array.from(root.querySelectorAll(sel));
  const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const reduceMotion = matchMedia('(prefers-reduced-motion: reduce)');
  const pinned = matchMedia('(min-width: 60rem)');
  const NBSP = ' ';

  // One polite live region for results that sighted visitors see change.
  const live = document.createElement('p');
  live.className = 'visually-hidden';
  live.setAttribute('role', 'status');
  document.body.append(live);

  /* Copy buttons */
  $$('[data-copy]').forEach((btn) => {
    const label = btn.textContent;
    let timer = 0;
    btn.addEventListener('click', async () => {
      const source = $(btn.dataset.copy);
      if (!source) return;
      const text = source.textContent.trim();
      let copied = false;
      try {
        await navigator.clipboard.writeText(text);
        copied = true;
      } catch {
        const range = document.createRange();
        range.selectNodeContents(source);
        const selection = getSelection();
        selection.removeAllRanges();
        selection.addRange(range);
      }
      btn.textContent = copied ? 'Copied' : 'Selected';
      btn.dataset.state = copied ? 'copied' : 'selected';
      live.textContent = copied ? 'Command copied' : 'Command selected, press Ctrl+C to copy';
      clearTimeout(timer);
      timer = setTimeout(() => {
        btn.textContent = label;
        delete btn.dataset.state;
      }, 1600);
    });
  });

  /* The benchmark, replayed at the speed it was measured.
     oz: 444.5 ms in total, the first spawn 79.7 ms (it captures the baseline).
     git: 22.78 s in total, 1.139 s per worktree plus cache copy. */
  const race = $('#race');
  if (race) {
    const navStatus = $('#nav-status');
    const replay = $('#race-replay');
    const CELLS = 20;
    const specs = {
      oz: {
        total: 444.5,
        first: 79.7,
        disk: 1.84,
        unit: 'MiB',
        time: (ms) => `${ms.toFixed(1)}${NBSP}ms`,
      },
      git: {
        total: 22780,
        first: 1139,
        disk: 97.96,
        unit: 'GiB',
        time: (ms) => `${(ms / 1000).toFixed(2)}${NBSP}s`,
      },
    };
    const lanes = Object.fromEntries(Object.entries(specs).map(([name, spec]) => {
      const el = $(`[data-lane="${name}"]`, race);
      const rest = (spec.total - spec.first) / (CELLS - 1);
      const ends = Array.from({ length: CELLS }, (_, i) => spec.first + rest * i);
      return [name, {
        ...spec,
        ends,
        cells: $$('.lane__cells i', el),
        painted: new Array(CELLS).fill(-1),
        read: {
          time: $('[data-read="time"]', el),
          disk: $('[data-read="disk"]', el),
          count: $('[data-read="count"]', el),
        },
        done: 0,
        diskNow: 0,
      }];
    }));

    const paint = (lane, t) => {
      let sum = 0;
      let done = 0;
      for (let i = 0; i < CELLS; i += 1) {
        const start = i === 0 ? 0 : lane.ends[i - 1];
        const p = Math.min(1, Math.max(0, (t - start) / (lane.ends[i] - start)));
        sum += p;
        if (p === 1) done += 1;
        if (lane.painted[i] !== p) {
          lane.cells[i].style.setProperty('--p', p.toFixed(3));
          lane.painted[i] = p;
        }
      }
      lane.done = done;
      lane.diskNow = lane.disk * (sum / CELLS);
      lane.read.time.textContent = lane.time(Math.min(t, lane.total));
      lane.read.disk.textContent = `${lane.diskNow.toFixed(2)}${NBSP}${lane.unit}`;
      lane.read.count.textContent = `${done} of ${CELLS}`;
    };

    let state = 'idle';
    let startedAt = 0;
    let frameId = 0;
    let inView = true;
    let statusTimer = 0;

    const showStatus = (text) => {
      if (!navStatus) return;
      navStatus.textContent = text;
      navStatus.classList.toggle('is-on', Boolean(text) && !inView);
    };

    const finish = () => {
      cancelAnimationFrame(frameId);
      state = 'done';
      race.dataset.state = 'done';
      paint(lanes.oz, lanes.oz.total);
      paint(lanes.git, lanes.git.total);
      replay.textContent = 'Replay';
      showStatus(`git finished in 22.78${NBSP}s. oz took 444.5${NBSP}ms.`);
      clearTimeout(statusTimer);
      statusTimer = setTimeout(() => showStatus(''), 7000);
    };

    const frame = (now) => {
      const t = now - startedAt;
      if (t >= lanes.git.total) {
        finish();
        return;
      }
      paint(lanes.oz, t);
      paint(lanes.git, t);
      const git = lanes.git;
      showStatus(`git is still copying: ${git.done} of ${CELLS} workspaces, ${git.diskNow.toFixed(1)}${NBSP}GiB`);
      frameId = requestAnimationFrame(frame);
    };

    const start = () => {
      cancelAnimationFrame(frameId);
      clearTimeout(statusTimer);
      state = 'running';
      race.dataset.state = 'running';
      replay.hidden = false;
      replay.textContent = 'Skip to result';
      paint(lanes.oz, 0);
      paint(lanes.git, 0);
      // A short beat so the eye is on the figure before the oz lane is over.
      setTimeout(() => {
        if (state !== 'running') return;
        startedAt = performance.now();
        frameId = requestAnimationFrame(frame);
      }, 700);
    };

    replay.addEventListener('click', () => (state === 'running' ? finish() : start()));

    new IntersectionObserver(([entry]) => {
      inView = entry.isIntersecting;
      if (navStatus) navStatus.classList.toggle('is-on', !inView && Boolean(navStatus.textContent));
      if (inView && state === 'idle') {
        if (reduceMotion.matches) {
          state = 'done';
          race.dataset.state = 'done';
          replay.hidden = false;
        } else {
          start();
        }
      }
    }, { threshold: 0.35 }).observe(race);
  }

  /* Terminal playback: commands type in, output follows. Nothing changes
     size while it plays, the untyped part of a command is only hidden. */
  const runs = new WeakMap();
  const token = (screen) => runs.get(screen) || 0;
  const cancel = (screen) => runs.set(screen, token(screen) + 1);

  // A command is set as one unbreakable piece per argument, so a line never
  // breaks inside a flag. Each piece holds a typed part and a hidden rest.
  const setCommand = (span, text) => {
    span.dataset.text = text;
    const parts = [];
    text.split(' ').forEach((word, i) => {
      if (i) parts.push(' ');
      const arg = document.createElement('span');
      arg.className = 'arg';
      const typed = document.createElement('span');
      typed.textContent = word;
      const rest = document.createElement('span');
      rest.className = 'cmd__rest';
      arg.append(typed, rest);
      parts.push(arg);
    });
    span.replaceChildren(...parts);
  };

  const showTyped = (span, count) => {
    let seen = 0;
    $$('.arg', span).forEach((arg) => {
      const word = arg.textContent;
      const n = Math.min(word.length, Math.max(0, count - seen));
      arg.firstChild.textContent = word.slice(0, n);
      arg.lastChild.textContent = word.slice(n);
      seen += word.length + 1;
    });
  };

  $$('.cmd__text').forEach((span) => setCommand(span, span.textContent));

  const settle = (blk) => {
    $$('.is-wait', blk).forEach((el) => el.classList.remove('is-wait'));
    $$('.cmd__text', blk).forEach((span) => showTyped(span, Infinity));
  };

  const typeCommand = async (cmd, screen, mine) => {
    const span = $('.cmd__text', cmd);
    const length = span.dataset.text.length;
    const step = Math.max(1, Math.ceil(length / 34));
    for (let i = 0; i < length; i += step) {
      if (token(screen) !== mine) return;
      showTyped(span, i);
      await wait(16);
    }
    showTyped(span, Infinity);
  };

  const scrollTerm = (screen, blk) => {
    const term = $('.term', screen);
    term.scrollTo({ top: Math.max(0, blk.offsetTop - 12), behavior: reduceMotion.matches ? 'auto' : 'smooth' });
  };

  const playBlocks = async (screen, blocks, follow = true) => {
    cancel(screen);
    const mine = token(screen);
    if (reduceMotion.matches) {
      blocks.forEach(settle);
      if (follow && blocks[0]) scrollTerm(screen, blocks[0]);
      return true;
    }
    blocks.forEach((blk) => {
      settle(blk);
      $$('.cmd, .out, .note', blk).forEach((el) => el.classList.add('is-wait'));
    });
    for (const blk of blocks) {
      if (follow) scrollTerm(screen, blk);
      for (const el of $$('.cmd, .out, .note', blk)) {
        if (token(screen) !== mine) return false;
        el.classList.remove('is-wait');
        if (el.classList.contains('cmd')) {
          await typeCommand(el, screen, mine);
          await wait(160);
        } else {
          await wait(el.classList.contains('note') ? 520 : 240);
        }
      }
    }
    return token(screen) === mine;
  };

  /* The pinned session */
  const moves = $$('.move');
  const screens = moves.map((move) => $('.screen', move));
  const played = new WeakSet();
  const gated = new Map();

  screens.forEach((screen) => {
    const btn = $('[data-advance]', screen);
    if (!btn || reduceMotion.matches) return;
    const blocks = $$('.blk[data-gate]', screen);
    const info = {
      btn,
      blocks,
      labels: btn.dataset.labels.split('|'),
      max: Math.max(...blocks.map((blk) => Number(blk.dataset.gate))),
      step: 0,
      touched: false,
      timer: 0,
    };
    gated.set(screen, info);
    blocks.forEach((blk) => { blk.hidden = Number(blk.dataset.gate) > 0; });
    btn.closest('.screen__bar').hidden = false;
    btn.addEventListener('click', () => advance(screen, true));
  });

  const queueNext = (screen) => {
    const info = gated.get(screen);
    if (!info || info.touched || info.step >= info.max) return;
    clearTimeout(info.timer);
    info.timer = setTimeout(() => {
      // Below the pinned layout a new block adds height, so only grow a
      // terminal the visitor is looking at.
      const box = screen.getBoundingClientRect();
      if (pinned.matches || (box.bottom > 0 && box.top < innerHeight)) advance(screen, false);
    }, 2200);
  };

  async function advance(screen, byUser) {
    const info = gated.get(screen);
    clearTimeout(info.timer);
    if (byUser) info.touched = true;
    if (info.step >= info.max) {
      cancel(screen);
      info.step = 0;
      info.blocks.forEach((blk) => {
        settle(blk);
        blk.hidden = Number(blk.dataset.gate) > 0;
      });
      info.btn.textContent = info.labels[0];
      $('.term', screen).scrollTo({ top: 0 });
      return;
    }
    info.step += 1;
    info.btn.textContent = info.labels[info.step];
    const next = info.blocks.filter((blk) => Number(blk.dataset.gate) === info.step);
    next.forEach((blk) => { blk.hidden = false; });
    if (await playBlocks(screen, next)) queueNext(screen);
  }

  const play = async (index) => {
    const screen = screens[index];
    if (!screen || played.has(screen)) return;
    played.add(screen);
    const first = $$('.blk[data-gate="0"]', screen);
    // The first blocks start at the top of the terminal, so nothing to follow.
    if (await playBlocks(screen, first, false)) queueNext(screen);
  };

  const activate = (index) => {
    moves.forEach((move, i) => {
      move.classList.toggle('is-active', i === index);
      screens[i].classList.toggle('is-active', i === index);
    });
    if (pinned.matches) play(index);
  };

  if (moves.length) {
    const texts = moves.map((move) => $('.move__text', move));
    // Whichever move's text crosses the middle of the viewport owns the screen.
    const atCentre = new IntersectionObserver((entries) => {
      entries.forEach((entry) => {
        if (entry.isIntersecting) activate(texts.indexOf(entry.target));
      });
    }, { rootMargin: '-45% 0px -45% 0px' });
    texts.forEach((text) => atCentre.observe(text));

    // Below the pinned layout each screen sits under its own text, so it
    // plays when it comes into view instead.
    const whenSeen = new IntersectionObserver((entries) => {
      entries.forEach((entry) => {
        if (entry.isIntersecting && !pinned.matches) play(screens.indexOf(entry.target));
      });
    }, { threshold: 0.3 });
    screens.forEach((screen) => whenSeen.observe(screen));
  }

  /* Move 1: pick files and hunks by ID, then commit them. Every result is
     the output oz printed for that exact combination. */
  const data = window.OZ_SESSION;
  const statusOut = $('#pick-status');
  if (data && statusOut) {
    const screen = statusOut.closest('.screen');
    const units = ['f1', 'f2', 'h3a', 'h3b', 'h3c', 'h3d'];
    const fileHunks = { f3: ['h3a', 'h3b', 'h3c', 'h3d'] };
    const selected = new Set(['f2', 'h3a', 'h3b', 'h3d']);
    const message = 'feat(auth): verify PKCE code verifier';
    const bar = $('#pick-bar');
    const cmdOut = $('#pick-cmd');
    const runBtn = $('#pick-run');
    const result = $('#pick-result');
    const peekBlock = $('#pick-peek');
    let ran = false;

    $$('.id', statusOut).forEach((span) => {
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'id';
      btn.textContent = span.textContent;
      Object.assign(btn.dataset, span.dataset);
      span.replaceWith(btn);
    });
    const ids = $$('button.id', statusOut);

    const renderDiff = (text) => {
      const frag = document.createDocumentFragment();
      text.split('\n').forEach((line, i, all) => {
        const span = document.createElement('span');
        if (/^(diff |--- |\+\+\+ |@@)/.test(line)) span.className = 'd-meta';
        else if (line.startsWith('+')) span.className = 'd-add';
        else if (line.startsWith('-')) span.className = 'd-del';
        span.textContent = line + (i < all.length - 1 ? '\n' : '');
        frag.append(span);
      });
      return frag;
    };

    const peek = (hunk) => {
      setCommand($('#pick-peek-cmd'), `oz diff ${hunk}`);
      $('#pick-peek-out').replaceChildren(renderDiff(data.diff[hunk]));
    };

    const isOn = (pick) => (fileHunks[pick] ? fileHunks[pick].every((h) => selected.has(h)) : selected.has(pick));
    const key = () => units.filter((unit) => selected.has(unit)).join(' ');
    const command = () => `oz commit -m "${message}" --pick ${key()}`.trim();

    const sync = () => {
      ids.forEach((btn) => {
        btn.setAttribute('aria-pressed', String(isOn(btn.dataset.pick)));
        btn.disabled = ran;
      });
      setCommand(cmdOut, command());
      cmdOut.classList.toggle('is-wait', ran);
      runBtn.disabled = !ran && selected.size === 0;
      runBtn.textContent = ran ? 'Reset' : 'Run commit';
    };

    ids.forEach((btn) => {
      btn.setAttribute('aria-label', `${btn.dataset.pick.startsWith('f') ? 'File' : 'Hunk'} ${btn.textContent}`);
      btn.addEventListener('click', () => {
        const pick = btn.dataset.pick;
        const group = fileHunks[pick] || [pick];
        const turnOff = isOn(pick);
        group.forEach((unit) => (turnOff ? selected.delete(unit) : selected.add(unit)));
        if (btn.dataset.peek) peek(btn.dataset.peek);
        sync();
      });
    });

    runBtn.addEventListener('click', () => {
      if (ran) {
        ran = false;
        cancel(screen);
        settle(result);
        result.hidden = true;
        peekBlock.hidden = false;
        sync();
        $('.term', screen).scrollTo({ top: 0, behavior: reduceMotion.matches ? 'auto' : 'smooth' });
        return;
      }
      const entry = data.pick[key()];
      if (!entry) return;
      ran = true;
      setCommand($('#pick-result-cmd'), command());
      // The diff has done its job once the commit runs, and without it the
      // status and the result fit the terminal with no scrolling.
      peekBlock.hidden = true;
      $('#pick-result-out').textContent = entry[0];
      $('#pick-after-out').textContent = entry[1];
      result.hidden = false;
      live.textContent = entry[0];
      sync();
      playBlocks(screen, [result]);
    });

    peek('h3c');
    result.hidden = true;
    bar.hidden = false;
    sync();
  }

  /* Move 5: dim the lines a person wrote. A view on this page, not an oz flag. */
  const blameBtn = $('#blame-filter');
  if (blameBtn) {
    blameBtn.closest('.screen__bar').hidden = false;
    blameBtn.addEventListener('click', () => {
      const on = blameBtn.getAttribute('aria-pressed') !== 'true';
      blameBtn.setAttribute('aria-pressed', String(on));
      $('#blame-out').classList.toggle('is-ai-only', on);
    });
  }

  /* One reveal, used once per block */
  const reveals = $$('[data-reveal]');
  if (reveals.length) {
    const seen = new IntersectionObserver((entries) => {
      entries.forEach((entry) => {
        if (!entry.isIntersecting) return;
        entry.target.classList.add('is-in');
        seen.unobserve(entry.target);
      });
    }, { threshold: 0.12, rootMargin: '0px 0px -6% 0px' });
    reveals.forEach((el) => seen.observe(el));
  }

  /* Install methods, preselected for the visitor's system */
  const sw = $('[data-switch]');
  if (sw) {
    const tabs = $$('[role="tab"]', sw);
    const select = (tab, focus) => {
      tabs.forEach((other) => {
        const on = other === tab;
        other.setAttribute('aria-selected', String(on));
        other.tabIndex = on ? 0 : -1;
        document.getElementById(other.getAttribute('aria-controls')).hidden = !on;
      });
      if (focus) tab.focus();
    };
    tabs.forEach((tab, i) => {
      tab.addEventListener('click', () => select(tab, false));
      tab.addEventListener('keydown', (event) => {
        const move = { ArrowRight: 1, ArrowLeft: -1 }[event.key];
        if (move) {
          event.preventDefault();
          select(tabs[(i + move + tabs.length) % tabs.length], true);
        } else if (event.key === 'Home' || event.key === 'End') {
          event.preventDefault();
          select(tabs[event.key === 'Home' ? 0 : tabs.length - 1], true);
        }
      });
    });
    const platform = ((navigator.userAgentData && navigator.userAgentData.platform) || navigator.platform || '').toLowerCase();
    const onWindows = platform.includes('win');
    select(onWindows ? $('#tab-ps') : tabs[0], false);
    if (onWindows) $('#hero-cmd').textContent = $('#code-ps').textContent;
  }
})();
