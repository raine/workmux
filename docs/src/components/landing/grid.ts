// A terminal drawn as a grid of character cells, played as a scripted loop
// with a chapter bar, a loop clock and a keycap for the key just pressed.

export type Seg = [string, string] | { svg: string; cls: string; w?: number };
export type Line = Seg[];
export type Step = [number, () => number | void];

export const FONT_FAMILY = "Iosevka Live";

export const esc = (s: string) =>
  s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
export const cw = (ch: string) => {
  const c = ch.codePointAt(0)!;
  return c >= 0x1f000 || c === 0x2705 ? 2 : 1;
};
export const strW = (s: string) => {
  let n = 0;
  for (const ch of s) n += cw(ch);
  return n;
};
export const segW = (s: Seg) => (Array.isArray(s) ? strW(s[0]) : (s.w ?? 1));
export const lineW = (l: Line) => l.reduce((n, s) => n + segW(s), 0);
export const sp = (n: number, cls = ""): Seg => [
  " ".repeat(Math.max(0, n)),
  cls,
];
export const clip = (s: string, w: number) =>
  strW(s) <= w ? s : s.slice(0, Math.max(0, w - 1)).trimEnd() + "…";

// Clip or pad a line to exactly `width` cells.
export function fit(line: Line, width: number): Line {
  const out: Line = [];
  let used = 0;
  for (const s of line) {
    if (used >= width) break;
    if (!Array.isArray(s)) {
      const w = s.w ?? 1;
      if (used + w > width) break;
      out.push(s);
      used += w;
      continue;
    }
    let txt = "";
    for (const ch of s[0]) {
      const w = cw(ch);
      if (used + w > width) break;
      txt += ch;
      used += w;
    }
    if (txt) out.push([txt, s[1]]);
  }
  if (used < width) out.push(sp(width - used));
  return out;
}

// Lay segments out left and right in `width` cells.
export const spread = (left: Line, right: Line, width: number): Line => [
  ...left,
  sp(width - lineW(left) - lineW(right)),
  ...right,
];

// One span per run of ASCII text in a class; every other glyph gets a cell
// box of its own so wide and fallback glyphs never shift the columns.
export function html(line: Line, svgs: Record<string, string>): string {
  let out = "";
  let runCls = "";
  let runTxt = "";
  let runW = 0;
  const flush = () => {
    if (runW)
      out += `<span class="${runCls}" style="width:${runW}ch">${esc(runTxt)}</span>`;
    runTxt = "";
    runW = 0;
  };
  for (const s of line) {
    if (!Array.isArray(s)) {
      flush();
      out += `<span class="${s.cls} g" style="width:${s.w ?? 1}ch">${svgs[s.svg]}</span>`;
      continue;
    }
    const [txt, cls] = s;
    for (const ch of txt) {
      if (ch.codePointAt(0)! < 0x80) {
        if (cls !== runCls) flush();
        runCls = cls;
        runTxt += ch;
        runW++;
      } else {
        flush();
        const w = cw(ch);
        out += `<span class="${cls} g${w === 2 ? " e" : ""}" style="width:${w}ch"><i>${ch}</i></span>`;
      }
    }
  }
  flush();
  return out;
}

export function wrapPlain(text: string, width: number): string[] {
  const out: string[] = [];
  let cur = "";
  for (const w of text.split(" ")) {
    if (cur && cur.length + 1 + w.length > width) {
      out.push(cur);
      cur = w;
    } else cur = cur ? `${cur} ${w}` : w;
  }
  if (cur) out.push(cur);
  return out;
}

export const fmtClock = (ms: number) => {
  const s = Math.floor(ms / 1000);
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
};

export const STEP = 25; // redraw interval

export abstract class GridLoop extends HTMLElement {
  abstract cols: number;
  abstract rowCount: number;
  abstract chapters: string[];
  t = 0;
  timer = 0;
  visible = false;
  rows: HTMLDivElement[] = [];
  cache: string[] = [];
  io?: IntersectionObserver;
  queue: { at: number; fn: () => void }[] = [];
  // Randomness comes from a seeded generator, so every loop runs to the same
  // length and the chapter bar can be measured once.
  seed = 1;
  loopT0 = 0;
  loops = 0;
  marks: number[] = [];
  total = 0;
  chapterEls: HTMLElement[] = [];
  timeEl?: HTMLElement;
  keyEl?: HTMLElement;
  keyLabel = "";
  keyUntil = 0;
  keyDownUntil = 0;
  // Set at the moment that best stands for the whole loop, the frame shown
  // when motion is reduced.
  poster = false;
  onVis = () => this.sync();
  refit = () => {};

  // Set up the loop's state and schedule its steps; starts with begin().
  abstract reset(): void;
  // The grid as one HTML string per row.
  abstract frame(): string[];
  // Per-tick changes that are not scheduled steps.
  advance() {}

  connectedCallback() {
    const grid = this.querySelector<HTMLElement>(".gl-grid")!;
    grid.style.setProperty("--cols", String(this.cols + 2.5));
    grid.textContent = "";
    for (let r = 0; r < this.rowCount; r++) {
      const row = document.createElement("div");
      row.className = "r";
      grid.append(row);
      this.rows.push(row);
    }
    this.keyEl = this.querySelector<HTMLElement>(".gl-key") ?? undefined;
    this.fitFont(grid);
    if (matchMedia("(prefers-reduced-motion: reduce)").matches) {
      for (const el of this.querySelectorAll<HTMLElement>(
        ".gl-key, .gl-chapters, .gl-time",
      ))
        el.hidden = true;
      this.reset();
      while (!this.poster && this.t < 120000) this.tick(false);
      this.paint();
      return;
    }
    this.measure();
    this.buildBar();
    this.reset();
    this.paint();
    this.io = new IntersectionObserver(([e]) => {
      this.visible = e.isIntersecting;
      // A grid first laid out hidden, in a closed tab, measures nothing.
      if (this.visible) this.refit();
      this.sync();
    });
    this.io.observe(this);
    document.addEventListener("visibilitychange", this.onVis);
  }

  disconnectedCallback() {
    this.io?.disconnect();
    document.removeEventListener("visibilitychange", this.onVis);
    clearInterval(this.timer);
    this.timer = 0;
  }

  sync() {
    const run = this.visible && !document.hidden;
    if (run && !this.timer)
      this.timer = window.setInterval(() => this.tick(), STEP);
    if (!run && this.timer) {
      clearInterval(this.timer);
      this.timer = 0;
    }
  }

  // Measure the advance width once the font is in, so the grid fills the
  // frame exactly.
  fitFont(grid: HTMLElement) {
    const measure = () => {
      if (!grid.offsetWidth) return;
      const probe = document.createElement("span");
      probe.textContent = "0".repeat(100);
      probe.style.cssText =
        "position:absolute;visibility:hidden;font-size:100px;line-height:1";
      grid.append(probe);
      const adv = probe.getBoundingClientRect().width / 10000;
      probe.remove();
      if (adv > 0) grid.style.setProperty("--adv", adv.toFixed(4));
    };
    this.refit = measure;
    measure();
    Promise.all(
      [`16px "${FONT_FAMILY}"`, `bold 16px "${FONT_FAMILY}"`].map((f) =>
        document.fonts.load(f),
      ),
    ).then(measure, () => {});
  }

  begin() {
    this.queue = [];
    this.seed = 1;
    this.loopT0 = this.t;
    this.loops++;
    this.mark(0);
    this.keyUntil = 0;
    this.poster = false;
  }

  // Play one loop without drawing to learn where each chapter starts and how
  // long the whole loop runs.
  measure() {
    this.reset();
    const n = this.loops;
    const t0 = this.t;
    while (this.loops === n && this.t - t0 < 180000) this.tick(false);
    this.total = this.t - t0;
  }

  buildBar() {
    this.timeEl = this.querySelector<HTMLElement>(".gl-time") ?? undefined;
    const box = this.querySelector(".gl-chapters");
    if (!box) return;
    this.chapterEls = this.chapters.map((label, i) => {
      const end = this.marks[i + 1] ?? this.total;
      const b = document.createElement("button");
      b.className = "gl-ch";
      b.type = "button";
      b.style.setProperty("--w", String(end - this.marks[i]));
      b.innerHTML = "<i></i>";
      b.title = label;
      b.setAttribute("aria-label", `Jump to ${label}`);
      b.addEventListener("click", () => this.seek(this.marks[i]));
      box.append(b);
      return b;
    });
  }

  // Jump to a point in the loop by replaying it from the start undrawn.
  seek(ms: number) {
    this.reset();
    while (this.t - this.loopT0 < ms) this.tick(false);
    this.paint();
  }

  mark(i: number) {
    this.marks[i] = this.t - this.loopT0;
  }

  rand(): number {
    this.seed = (this.seed * 16807) % 2147483647;
    return (this.seed - 1) / 2147483646;
  }

  key(label: string, hold = 700) {
    this.keyLabel = label;
    this.keyUntil = this.t + hold;
    this.keyDownUntil = this.t + 110;
  }

  later(ms: number, fn: () => void) {
    this.queue.push({ at: this.t + ms, fn });
    this.queue.sort((a, b) => a.at - b.at);
  }

  // Run steps in order; each waits its delay, plus whatever its function
  // returns.
  play(steps: Step[], then?: () => void) {
    const next = (i: number) => {
      if (i >= steps.length) return then?.();
      const [delay, fn] = steps[i];
      this.later(delay, () => {
        const extra = fn() || 0;
        if (extra) this.later(extra, () => next(i + 1));
        else next(i + 1);
      });
    };
    next(0);
  }

  tick(draw = true) {
    this.t += STEP;
    while (this.queue.length && this.queue[0].at <= this.t)
      this.queue.shift()!.fn();
    this.advance();
    if (draw) this.paint();
  }

  drawBar() {
    if (!this.timeEl) return;
    const el = Math.min(this.t - this.loopT0, this.total);
    this.chapterEls.forEach((b, i) => {
      const start = this.marks[i];
      const end = this.marks[i + 1] ?? this.total;
      const p = Math.max(0, Math.min(1, (el - start) / (end - start)));
      b.style.setProperty("--p", `${(p * 100).toFixed(1)}%`);
    });
    const text = `${fmtClock(el)} / ${fmtClock(this.total)}`;
    if (this.timeEl.textContent !== text) this.timeEl.textContent = text;
  }

  drawKey() {
    const el = this.keyEl;
    if (!el) return;
    const on = this.t < this.keyUntil;
    if (on && el.textContent !== this.keyLabel) el.textContent = this.keyLabel;
    el.toggleAttribute("data-on", on);
    el.toggleAttribute("data-down", this.t < this.keyDownUntil);
  }

  paint() {
    this.drawBar();
    this.drawKey();
    const rows = this.frame();
    for (let r = 0; r < this.rowCount; r++) {
      const h = rows[r] ?? "";
      if (h !== this.cache[r]) {
        this.cache[r] = h;
        this.rows[r].innerHTML = h;
      }
    }
  }
}
