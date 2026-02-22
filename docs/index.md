---
layout: home
description: The zero-friction workflow for git worktrees, jj workspaces, and tmux, kitty, WezTerm, or Zellij
---

<div class="mono-editorial">

<section class="ed-hero">
  <div class="ed-hero-bg">
    <div class="ed-hero-glow"></div>
    <div class="ed-hero-grid"></div>
  </div>
  <div class="ed-container ed-hero-inner">
    <div class="ed-hero-text">
      <span class="ed-hero-name">workmux</span>
      <h1 class="ed-hero-headline">Ship features in parallel from terminal.</h1>
      <p class="ed-hero-tagline">workmux creates an isolated worktree and tmux tab per task. Run multiple agents in parallel without conflicts.</p>
      <div class="ed-hero-actions">
        <a href="/guide/quick-start" class="ed-btn-primary">Get started</a>
        <a href="https://github.com/raine/workmux" class="ed-btn-github">GitHub</a>
      </div>
    </div>
    <div class="ed-hero-logo">
      <img src="/icon.svg" alt="" class="ed-logo-light">
      <img src="/icon-dark.svg" alt="" class="ed-logo-dark">
    </div>
  </div>
</section>

<section class="ed-why">
  <div class="ed-container">
    <div class="ed-accent-rule"></div>
    <span class="ed-section-label">Why workmux?</span>
    <div class="ed-why-grid">
      <div class="ed-why-item">
        <div class="ed-why-header">
          <span class="ed-why-icon"><svg xmlns="http://www.w3.org/2000/svg" width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="6" y1="3" x2="6" y2="15"></line><circle cx="18" cy="6" r="3"></circle><circle cx="6" cy="18" r="3"></circle><path d="M18 9a9 9 0 0 1-9 9"></path></svg></span>
          <h3>Parallel workflows</h3>
        </div>
        <p>Work on multiple features, hotfixes, or AI agents at the same time. No stashing, no branch switching, no conflicts.</p>
      </div>
      <div class="ed-why-item">
        <div class="ed-why-header">
          <span class="ed-why-icon"><svg xmlns="http://www.w3.org/2000/svg" width="20" height="20" viewBox="0 0 16 16"><path fill="currentColor" d="M1.75 1.5a.25.25 0 0 0-.25.25v12.5c0 .138.112.25.25.25h5.5v-13zm7 0v5.75h5.75v-5.5a.25.25 0 0 0-.25-.25zm5.75 7.25H8.75v5.75h5.5a.25.25 0 0 0 .25-.25zM0 1.75C0 .784.784 0 1.75 0h12.5C15.216 0 16 .784 16 1.75v12.5A1.75 1.75 0 0 1 14.25 16H1.75A1.75 1.75 0 0 1 0 14.25z"/></svg></span>
          <h3>One window per task</h3>
        </div>
        <p>A natural mental model. Each has its own terminal state, editor session, and dev server. Context switching is switching tabs.</p>
      </div>
      <div class="ed-why-item">
        <div class="ed-why-header">
          <span class="ed-why-icon"><svg xmlns="http://www.w3.org/2000/svg" width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="4 17 10 11 4 5"></polyline><line x1="12" y1="19" x2="20" y2="19"></line></svg></span>
          <h3>Terminal workflow</h3>
        </div>
        <p>Build on your familiar terminal setup instead of yet another agentic GUI that won't exist next year. Your tools, your muscle memory.</p>
      </div>
    </div>
  </div>
</section>

<section class="ed-pain-points">
  <div class="ed-container">
    <div class="ed-accent-rule"></div>
    <span class="ed-section-label">Worktree pain points, solved</span>
    <p class="ed-section-desc">Git worktrees (and jj workspaces) are powerful, but managing them manually is painful. workmux automates the rough edges.</p>
    <div class="ed-pain-points-list">
      <div class="ed-pain-point">
        <h3>"You need to reinstall everything"</h3>
        <p>New worktrees are clean checkouts with no <code>.env</code>, no <code>node_modules</code>, no dev server. workmux can <a href="/guide/configuration#file-operations">copy config files, symlink dependencies</a>, and <a href="/guide/configuration#lifecycle-hooks">run setup commands</a> on creation. Configure once, reuse everywhere.</p>
      </div>
      <div class="ed-pain-point">
        <h3>"You need to clean up after"</h3>
        <p><code>workmux merge</code> handles the full lifecycle: merge the branch, delete the worktree, close the tmux window, remove the local branch. One command. Or go next level and use the <a href="/guide/skills#merge"><code>/merge</code> skill</a> to let your agent commit, rebase, and merge autonomously.</p>
      </div>
      <div class="ed-pain-point">
        <h3>"Conflicts arise on merge"</h3>
        <p>Conflicts are inherent to git when changes overlap, and worktrees don't change that. But your agent can handle them. The <a href="/guide/skills#merge"><code>/merge</code></a> skill tells your agent to rebase onto the base branch, review the upstream changes, and resolve conflicts by understanding both sides. No manual conflict resolution needed in most cases. See <a href="/guide/workflows#finishing-work">finishing work</a>.</p>
      </div>
    </div>
  </div>
</section>

<section class="ed-demo">
  <div class="ed-container ed-align-right">
    <div class="ed-accent-rule ed-accent-rule-right"></div>
    <span class="ed-section-label">See it in action</span>
    <p class="ed-section-desc">Spin up worktrees, develop in parallel, merge and clean up.</p>
  </div>
  <div class="ed-container ed-showcase">
    <div class="ed-window-glow"></div>
    <div class="terminal-window">
      <div class="terminal-header">
        <div class="window-controls">
          <span class="control red"></span>
          <span class="control yellow"></span>
          <span class="control green"></span>
        </div>
        <div class="window-title">workmux demo</div>
      </div>
      <div class="video-container">
        <video src="/demo.mp4" controls muted playsinline preload="metadata"></video>
        <button type="button" class="video-play-button" aria-label="Play video"></button>
      </div>
    </div>
  </div>
</section>

<section class="ed-sandbox">
  <div class="ed-container">
    <div class="ed-accent-rule"></div>
    <span class="ed-section-label">Sandbox</span>
    <h2 class="ed-sandbox-headline">Run agents in YOLO mode.</h2>
    <p class="ed-sandbox-desc">Enable sandboxing to run agents in isolated containers or Lima VMs scoped to the worktree. Host keys, creds, and files stay isolated while agents operate inside the worktree.</p>
    <a href="/guide/sandbox/" class="ed-sandbox-link">Learn more →</a>
  </div>
</section>

<section class="ed-dashboard">
  <div class="ed-container ed-align-right">
    <div class="ed-accent-rule ed-accent-rule-right"></div>
    <span class="ed-section-label">Monitor your agents</span>
    <p class="ed-section-desc">A tmux popup dashboard to track progress across all agents.</p>
  </div>
  <div class="ed-container ed-showcase">
    <div class="ed-window-glow"></div>
    <div class="terminal-window">
      <div class="terminal-header">
        <div class="window-controls">
          <span class="control red"></span>
          <span class="control yellow"></span>
          <span class="control green"></span>
        </div>
        <div class="window-title">workmux dashboard</div>
      </div>
      <img src="/dashboard.webp" alt="workmux dashboard" class="dashboard-img">
    </div>
  </div>
</section>

<section class="ed-workflows">
  <div class="ed-container">
    <div class="ed-accent-rule"></div>
    <span class="ed-section-label">Workflows</span>
    <h2 class="ed-workflows-headline">From a single command to agent orchestration skills.</h2>
    <div class="ed-modes">
      <div class="ed-mode">
        <h3 class="ed-mode-title">Solo</h3>
        <p class="ed-mode-cmd">workmux add -A "Add cursor-based pagination to /api/users"</p>
        <p class="ed-mode-benefit">One command creates a branch, worktree, and starts an agent with your prompt in a new tab.</p>
      </div>
      <div class="ed-mode">
        <h3 class="ed-mode-title">Delegated</h3>
        <p class="ed-mode-cmd"><code>/worktree</code> Implement the caching layer</p>
        <p class="ed-mode-benefit">From inside an agent, spin off a subtask to a new worktree with full context.</p>
      </div>
      <div class="ed-mode">
        <h3 class="ed-mode-title">Coordinated</h3>
        <p class="ed-mode-cmd"><code>/coordinator</code> Break down the auth refactor into parallel tasks</p>
        <p class="ed-mode-benefit">One agent spawns, monitors, and merges multiple worktree agents.</p>
      </div>
    </div>
    <a href="/guide/workflows" class="ed-workflows-link">Learn more →</a>
  </div>
</section>

<section class="ed-testimonials">
  <div class="ed-container">
    <div class="ed-accent-rule"></div>
    <span class="ed-section-label">What people are saying</span>
    <div class="ed-testimonial-list">
      <div class="ed-testimonial">
        <blockquote>"I've been using (and loving) workmux which brings together tmux, git worktrees, and CLI agents into an opinionated workflow."</blockquote>
        <cite>— @Coolin96 <a href="https://news.ycombinator.com/item?id=46029809">via Hacker News</a></cite>
      </div>
      <div class="ed-testimonial">
        <blockquote>"Thank you so much for your work with workmux! It's a tool I've been wanting to exist for a long time."</blockquote>
        <cite>— @rstacruz <a href="https://github.com/raine/workmux/issues/2">via GitHub</a></cite>
      </div>
      <div class="ed-testimonial">
        <blockquote>"It's become my daily driver — the perfect level of abstraction over tmux + git, without getting in the way or obscuring the underlying tooling."</blockquote>
        <cite>— @cisaacstern <a href="https://github.com/raine/workmux/issues/33">via GitHub</a></cite>
      </div>
    </div>
  </div>
</section>

<section class="ed-cta">
  <div class="ed-container ed-align-center">
    <div class="ed-cta-actions">
      <a href="/guide/quick-start" class="ed-btn-primary">
        Get started
        <svg xmlns="http://www.w3.org/2000/svg" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M5 12h14"/><path d="m12 5 7 7-7 7"/></svg>
      </a>
      <a href="https://github.com/raine/workmux" class="ed-btn-secondary">
        <svg xmlns="http://www.w3.org/2000/svg" width="18" height="18" viewBox="0 0 24 24" fill="currentColor"><path d="M12 0c-6.626 0-12 5.373-12 12 0 5.302 3.438 9.8 8.207 11.387.599.111.793-.261.793-.577v-2.234c-3.338.726-4.033-1.416-4.033-1.416-.546-1.387-1.333-1.756-1.333-1.756-1.089-.745.083-.729.083-.729 1.205.084 1.839 1.237 1.839 1.237 1.07 1.834 2.807 1.304 3.492.997.107-.775.418-1.305.762-1.604-2.665-.305-5.467-1.334-5.467-5.931 0-1.311.469-2.381 1.236-3.221-.124-.303-.535-1.524.117-3.176 0 0 1.008-.322 3.301 1.23.957-.266 1.983-.399 3.003-.404 1.02.005 2.047.138 3.006.404 2.291-1.552 3.297-1.23 3.297-1.23.653 1.653.242 2.874.118 3.176.77.84 1.235 1.911 1.235 3.221 0 4.609-2.807 5.624-5.479 5.921.43.372.823 1.102.823 2.222v3.293c0 .319.192.694.801.576 4.765-1.589 8.199-6.086 8.199-11.386 0-6.627-5.373-12-12-12z"/></svg>
        View on GitHub
      </a>
    </div>
  </div>
</section>

</div>

<script setup>
import { onMounted } from 'vue'
import { data as stars } from './stars.data'

onMounted(() => {
  // Add star count to GitHub hero button
  if (stars) {
    const btn = document.querySelector('.ed-btn-github')
    if (btn && !btn.querySelector('.ed-star-count')) {
      const formatted = stars >= 1000 ? (stars / 1000).toFixed(1) + 'k' : stars
      const span = document.createElement('span')
      span.className = 'ed-star-count'
      span.textContent = `★ ${formatted}`
      btn.appendChild(span)
    }
  }

  const container = document.querySelector('.video-container')
  const video = container?.querySelector('video')
  const playBtn = container?.querySelector('.video-play-button')

  if (video && playBtn) {
    playBtn.addEventListener('click', () => {
      video.play()
      container.classList.add('playing')
    })

    video.addEventListener('pause', () => {
      container.classList.remove('playing')
    })

    video.addEventListener('play', () => {
      container.classList.add('playing')
    })
  }
})
</script>

<style>
@import url('https://fonts.cdnfonts.com/css/cabinet-grotesk');

/* Override VitePress home defaults */
.VPHome {
  padding-bottom: 0 !important;
  margin-bottom: 0 !important;
}

/* ===== Base ===== */
.mono-editorial {
  --ed-container: 900px;
  --ed-wide: 960px;
  --ed-accent: var(--vp-c-brand-1);
  --ed-font-display: 'Cabinet Grotesk', system-ui, -apple-system, sans-serif;
  --ed-font-mono: var(--vp-font-family-mono);
}

/* Reset VitePress defaults inside editorial */
.mono-editorial h1,
.mono-editorial h2,
.mono-editorial h3 {
  border: none;
  padding: 0;
  margin: 0;
}

.mono-editorial p {
  margin: 0;
}

.mono-editorial blockquote {
  border: none;
  padding: 0;
  margin: 0;
  background: none;
}

/* ===== Containers ===== */
.ed-container {
  max-width: var(--ed-container);
  margin-left: auto;
  margin-right: auto;
  padding-left: 2rem;
  padding-right: 2rem;
}

.ed-wide {
  max-width: var(--ed-wide);
  margin-left: auto;
  margin-right: auto;
  padding-left: 1.5rem;
  padding-right: 1.5rem;
}

.ed-align-right {
  text-align: right;
}

.ed-align-center {
  text-align: center;
}

/* ===== Accent rules ===== */
.ed-accent-rule {
  width: 48px;
  height: 1px;
  background: var(--ed-accent);
  margin-bottom: 1.5rem;
}

.ed-accent-rule-right {
  margin-left: auto;
}

/* ===== Section labels ===== */
.ed-section-label {
  display: block;
  font-family: var(--ed-font-mono);
  font-size: 0.75rem;
  text-transform: uppercase;
  letter-spacing: 0.1em;
  color: var(--vp-c-text-2);
  margin-bottom: 2.5rem;
}

.ed-section-desc {
  font-size: 0.9375rem;
  line-height: 1.6;
  color: var(--vp-c-text-2);
  margin-top: -1.5rem !important;
  margin-bottom: 2.5rem !important;
}

/* ===== Hero ===== */
.ed-hero {
  position: relative;
  overflow: hidden;
  padding: 8rem 0 7rem;
}

.ed-hero-bg {
  position: absolute;
  inset: 0;
  pointer-events: none;
}

.ed-hero-glow {
  position: absolute;
  top: -20%;
  left: 50%;
  transform: translateX(-50%);
  width: 800px;
  height: 600px;
  background: radial-gradient(ellipse at center, var(--ed-accent) 0%, transparent 70%);
  opacity: 0.08;
  filter: blur(60px);
}

.ed-hero-grid {
  position: absolute;
  inset: 0;
  background-image:
    linear-gradient(var(--vp-c-divider) 1px, transparent 1px),
    linear-gradient(90deg, var(--vp-c-divider) 1px, transparent 1px);
  background-size: 60px 60px;
  opacity: 0.4;
  mask-image: radial-gradient(ellipse 80% 70% at 50% 30%, black, transparent);
  -webkit-mask-image: radial-gradient(ellipse 80% 70% at 50% 30%, black, transparent);
}

.ed-hero-inner {
  position: relative;
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 4rem;
}

.ed-hero-text {
  flex: 1;
  min-width: 0;
}

.ed-hero-logo {
  flex-shrink: 0;
  width: 200px;
}

.ed-logo-light,
.ed-logo-dark {
  width: 100%;
  height: auto;
}

.ed-logo-light { display: block; }
.ed-logo-dark { display: none; }
.dark .ed-logo-light { display: none; }
.dark .ed-logo-dark { display: block; }

.ed-hero-name {
  display: block;
  font-family: var(--ed-font-mono);
  font-size: 0.875rem;
  font-weight: 500;
  letter-spacing: 0.05em;
  color: var(--ed-accent);
  margin-bottom: 1.25rem;
}

.ed-hero-headline {
  font-family: var(--ed-font-display);
  font-size: clamp(3.5rem, 10vw, 7.5rem);
  font-weight: 800;
  line-height: 0.95;
  letter-spacing: -0.04em !important;
  color: var(--vp-c-text-1);
  margin-bottom: 2rem !important;
}

.ed-hero-tagline {
  font-family: var(--ed-font-mono);
  font-size: 0.875rem;
  line-height: 1.7;
  color: var(--vp-c-text-2);
  max-width: 440px;
  margin-bottom: 2.5rem !important;
}

.ed-hero-actions {
  display: flex;
  align-items: center;
  gap: 1.5rem;
}

/* ===== Buttons ===== */
.ed-btn-primary {
  display: inline-flex;
  align-items: center;
  gap: 0.5rem;
  padding: 0.65rem 1.4rem;
  background: var(--ed-accent);
  color: #fff !important;
  font-weight: 600;
  font-size: 0.875rem;
  border-radius: 6px;
  text-decoration: none !important;
  transition: opacity 0.2s;
}

.ed-btn-primary:hover {
  opacity: 0.85;
  color: #fff !important;
}

.ed-btn-github {
  display: inline-flex;
  align-items: center;
  gap: 0.5rem;
  font-size: 0.875rem;
  font-weight: 500;
  color: var(--vp-c-text-2) !important;
  text-decoration: none !important;
  transition: color 0.2s;
}

.ed-btn-github:hover {
  color: var(--vp-c-text-1) !important;
}

.ed-star-count {
  padding-left: 0.5rem;
  border-left: 1px solid var(--vp-c-divider);
  font-size: 0.85em;
  opacity: 0.7;
}

.ed-btn-secondary {
  display: inline-flex;
  align-items: center;
  gap: 0.5rem;
  padding: 0.65rem 1.4rem;
  background: var(--vp-c-bg-soft);
  color: var(--vp-c-text-1) !important;
  font-weight: 600;
  font-size: 0.875rem;
  border: 1px solid var(--vp-c-divider);
  border-radius: 6px;
  text-decoration: none !important;
  transition: border-color 0.2s;
}

.ed-btn-secondary:hover {
  border-color: var(--vp-c-text-3);
  color: var(--vp-c-text-1) !important;
}

/* ===== Why section ===== */
.ed-why {
  padding: 0 0 8rem;
}

.ed-why-grid {
  display: grid;
  grid-template-columns: repeat(3, minmax(0, 1fr));
  gap: 3rem;
}

.ed-why-header {
  display: flex;
  align-items: center;
  gap: 0.6rem;
  margin-bottom: 0.75rem;
}

.ed-why-icon {
  display: flex;
  align-items: center;
  justify-content: center;
  flex-shrink: 0;
  color: var(--ed-accent);
}

.ed-why-item h3 {
  font-family: var(--ed-font-display);
  font-size: 1.375rem;
  font-weight: 700;
  letter-spacing: -0.02em;
  line-height: 1.15;
  color: var(--vp-c-text-1);
}

.ed-why-item p {
  font-size: 0.875rem;
  line-height: 1.7;
  color: var(--vp-c-text-2);
}

/* ===== Pain points section ===== */
.ed-pain-points {
  padding: 0 0 8rem;
}

.ed-pain-points-list {
  display: flex;
  flex-direction: column;
}

.ed-pain-point {
  padding: 1.75rem 0;
  border-top: 1px solid var(--vp-c-divider);
}

.ed-pain-point:last-child {
  border-bottom: 1px solid var(--vp-c-divider);
}

.ed-pain-point h3 {
  font-family: var(--ed-font-mono);
  font-size: 0.9375rem;
  font-weight: 600;
  color: var(--vp-c-text-1);
  margin-bottom: 0.5rem !important;
}

.ed-pain-point p {
  font-size: 0.875rem;
  line-height: 1.7;
  color: var(--vp-c-text-2);
}

.ed-pain-point code {
  font-family: var(--ed-font-mono);
  font-size: 0.8125rem;
  background: var(--vp-c-bg-soft);
  border-radius: 4px;
  padding: 0.15em 0.4em;
}

/* ===== Demo section ===== */
.ed-demo {
  padding: 0 0 8rem;
}

/* ===== Code section ===== */
/* ===== Workflows section ===== */
.ed-workflows {
  padding: 0 0 8rem;
}

.ed-workflows-headline {
  font-family: var(--ed-font-display);
  font-size: clamp(1.75rem, 4vw, 2.5rem);
  font-weight: 800;
  letter-spacing: -0.03em;
  line-height: 1.1;
  color: var(--vp-c-text-1);
  margin-bottom: 3rem !important;
}

.ed-modes {
  display: flex;
  flex-direction: column;
}

.ed-mode {
  padding: 1.75rem 0;
  border-top: 1px solid var(--vp-c-divider);
}

.ed-mode:last-child {
  border-bottom: 1px solid var(--vp-c-divider);
}

.ed-mode-title {
  font-family: var(--ed-font-display);
  font-size: 1.125rem;
  font-weight: 700;
  letter-spacing: -0.01em;
  color: var(--vp-c-text-1);
  margin-bottom: 0.5rem !important;
}

.ed-mode-cmd {
  font-family: var(--ed-font-mono);
  font-size: 0.875rem;
  line-height: 1.6;
  color: var(--vp-c-text-2);
  margin-bottom: 0.375rem !important;
}

.ed-mode-cmd code {
  font-family: var(--ed-font-mono);
  font-size: 0.8125rem;
  background: var(--vp-c-bg-soft);
  border-radius: 4px;
  padding: 0.15em 0.4em;
}

.ed-mode-benefit {
  font-size: 0.875rem;
  line-height: 1.6;
  color: var(--vp-c-text-3);
}

.ed-workflows-link {
  display: inline-block;
  margin-top: 1.5rem;
  font-family: var(--ed-font-mono);
  font-size: 0.75rem;
  text-transform: uppercase;
  letter-spacing: 0.05em;
  color: var(--ed-accent) !important;
  text-decoration: none !important;
  transition: opacity 0.2s;
}

.ed-workflows-link:hover {
  opacity: 0.75;
}

/* ===== Sandbox section ===== */
.ed-sandbox {
  padding: 0 0 8rem;
}

.ed-sandbox-headline {
  font-family: var(--ed-font-display);
  font-size: clamp(1.75rem, 4vw, 2.5rem);
  font-weight: 700;
  letter-spacing: -0.03em;
  line-height: 1.15;
  color: var(--vp-c-text-1);
  margin-bottom: 1.25rem !important;
  max-width: 480px;
}

.ed-sandbox-desc {
  font-size: 0.9375rem;
  line-height: 1.7;
  color: var(--vp-c-text-2);
  margin-bottom: 1.5rem !important;
  max-width: 520px;
}

.ed-sandbox-points {
  list-style: none;
  padding: 0;
  margin: 0 0 1.5rem;
  max-width: 520px;
}

.ed-sandbox-points li {
  font-family: var(--ed-font-mono);
  font-size: 0.8125rem;
  color: var(--vp-c-text-2);
  padding: 0.4rem 0;
  border-bottom: 1px solid var(--vp-c-divider);
}

.ed-sandbox-points li:first-child {
  border-top: 1px solid var(--vp-c-divider);
}

.ed-sandbox-link {
  font-family: var(--ed-font-mono);
  font-size: 0.75rem;
  text-transform: uppercase;
  letter-spacing: 0.05em;
  color: var(--ed-accent) !important;
  text-decoration: none !important;
}

.ed-sandbox-link:hover {
  text-decoration: underline !important;
}

/* ===== Dashboard section ===== */
.ed-dashboard {
  padding: 0 0 8rem;
}

/* ===== Showcase glow ===== */
.ed-showcase {
  position: relative;
}

.ed-window-glow {
  position: absolute;
  top: 50%;
  left: 50%;
  transform: translate(-50%, -50%);
  width: 90%;
  height: 90%;
  background: var(--ed-accent);
  filter: blur(70px);
  opacity: 0.2;
  border-radius: 50%;
  z-index: 0;
  pointer-events: none;
}

.ed-showcase .terminal-window {
  position: relative;
  z-index: 1;
}

/* ===== Terminal windows ===== */
.terminal-window {
  background: #1a1a1a;
  border-radius: 10px;
  overflow: hidden;
  box-shadow: 0 25px 60px -15px rgba(0, 0, 0, 0.25);
}

.terminal-header {
  display: flex;
  align-items: center;
  justify-content: center;
  height: 36px;
  background: #252525;
  position: relative;
}

.window-controls {
  position: absolute;
  left: 14px;
  display: flex;
  gap: 7px;
}

.control {
  width: 11px;
  height: 11px;
  border-radius: 50%;
}

.control.red { background: #ff5f56; }
.control.yellow { background: #ffbd2e; }
.control.green { background: #27c93f; }

.window-title {
  font-family: var(--ed-font-mono);
  font-size: 0.75rem;
  color: rgba(255, 255, 255, 0.3);
}

.dashboard-img {
  display: block;
  width: 100%;
}

/* ===== Video ===== */
.video-container {
  position: relative;
}

.video-container video {
  display: block;
  width: 100%;
  cursor: pointer;
}

.video-play-button {
  position: absolute;
  top: 50%;
  left: 50%;
  transform: translate(-50%, -50%);
  width: 72px;
  height: 72px;
  border: none;
  border-radius: 50%;
  background: rgba(255, 255, 255, 0.12);
  backdrop-filter: blur(4px);
  cursor: pointer;
  transition: background 0.2s, transform 0.2s;
}

.video-play-button::before {
  content: '';
  position: absolute;
  top: 50%;
  left: 55%;
  transform: translate(-50%, -50%);
  border-style: solid;
  border-width: 12px 0 12px 20px;
  border-color: transparent transparent transparent white;
}

.video-play-button:hover {
  background: var(--ed-accent);
  transform: translate(-50%, -50%) scale(1.05);
}

.video-container.playing .video-play-button {
  display: none;
}

/* ===== Testimonials ===== */
.ed-testimonials {
  padding: 0 0 8rem;
}

.ed-testimonial-list {
  display: flex;
  flex-direction: column;
  gap: 4rem;
}

.ed-testimonial blockquote {
  font-size: 1.375rem;
  font-style: italic;
  line-height: 1.5;
  color: var(--vp-c-text-1);
  margin-bottom: 0.75rem !important;
}

.ed-testimonial cite {
  display: block;
  font-family: var(--ed-font-mono);
  font-style: normal;
  font-size: 0.8125rem;
  color: var(--vp-c-text-2);
}

.ed-testimonial cite a {
  color: var(--ed-accent);
  text-decoration: none;
}

.ed-testimonial cite a:hover {
  text-decoration: underline;
}

/* ===== CTA ===== */
.ed-cta {
  padding: 0 0 6rem;
}

.ed-cta-actions {
  display: flex;
  justify-content: center;
  align-items: center;
  gap: 1.5rem;
}

/* ===== Responsive ===== */
@media (max-width: 960px) {
  .ed-hero {
    padding: 5rem 0 5rem;
  }

  .ed-hero-inner {
    gap: 2rem;
  }

  .ed-hero-logo {
    width: 140px;
  }

  .ed-why-grid {
    grid-template-columns: 1fr;
    gap: 2rem;
  }

  .ed-align-right {
    text-align: left;
  }

  .ed-accent-rule-right {
    margin-left: 0;
  }

  .ed-why,
  .ed-pain-points,
  .ed-demo,
  .ed-workflows,
  .ed-sandbox,
  .ed-dashboard,
  .ed-testimonials {
    padding-bottom: 6rem;
  }

}

@media (max-width: 640px) {
  .ed-container {
    padding-left: 1.25rem;
    padding-right: 1.25rem;
  }

  .ed-wide {
    padding-left: 0.75rem;
    padding-right: 0.75rem;
  }

  .ed-hero {
    padding: 3.5rem 0 3.5rem;
  }

  .ed-hero-logo {
    display: none;
  }

  .ed-hero-actions {
    gap: 1rem;
  }

  .ed-why,
  .ed-pain-points,
  .ed-demo,
  .ed-sandbox,
  .ed-dashboard,
  .ed-testimonials {
    padding-bottom: 4rem;
  }

  .ed-workflows {
    padding-bottom: 4rem;
  }

  .ed-testimonial blockquote {
    font-size: 1.125rem;
  }

  .ed-cta-actions {
    flex-direction: column;
  }
}
</style>
