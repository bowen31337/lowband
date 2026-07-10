import gsap from "gsap";
import { ScrollTrigger } from "gsap/ScrollTrigger";
import { animate, hover, inView, press } from "motion";
import "./style.css";

gsap.registerPlugin(ScrollTrigger);

const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

/* ---------- Hero entrance (GSAP timeline) ---------- */
if (!reducedMotion) {
  gsap
    .timeline({ defaults: { ease: "power3.out", duration: 0.8 } })
    .from("[data-hero='badge']", { y: 16, autoAlpha: 0 })
    .from("[data-hero='title']", { y: 28, autoAlpha: 0 }, "-=0.5")
    .from("[data-hero='sub']", { y: 24, autoAlpha: 0 }, "-=0.55")
    .from("[data-hero='cta']", { y: 20, autoAlpha: 0 }, "-=0.55")
    .from("[data-hero='note']", { autoAlpha: 0 }, "-=0.4")
    .from("[data-hero='terminal']", { y: 36, autoAlpha: 0, duration: 1 }, "-=0.7");
}

/* ---------- Hero terminal: live link simulation ---------- */
const tierForKbps = (kbps: number): { label: string; cls: string } => {
  if (kbps < 48) return { label: "SURVIVAL", cls: "bg-rose-500/15 text-rose-400" };
  if (kbps < 128) return { label: "CONSTRAINED", cls: "bg-amber-400/15 text-amber-300" };
  if (kbps < 300) return { label: "COMFORTABLE", cls: "bg-signal/15 text-signal" };
  return { label: "FULL", cls: "bg-sky-400/15 text-sky-300" };
};

const bwEl = document.getElementById("hero-bw");
const tierEl = document.getElementById("hero-tier");
const meterEl = document.getElementById("hero-meter");
const meterLabel = document.getElementById("hero-meter-label");

if (bwEl && tierEl && meterEl && meterLabel && !reducedMotion) {
  // A flaky 3G link: wander between survival and comfortable, mostly constrained.
  const linkTrace = [64, 41, 58, 96, 142, 88, 52, 33, 70, 118, 64];
  let idx = 0;
  const state = { kbps: 64 };

  const step = () => {
    idx = (idx + 1) % linkTrace.length;
    gsap.to(state, {
      kbps: linkTrace[idx],
      duration: 2.4,
      ease: "sine.inOut",
      onUpdate: () => {
        const kbps = Math.round(state.kbps);
        bwEl.textContent = String(kbps);
        meterLabel.textContent = `${kbps} kbps`;
        meterEl.style.width = `${Math.min(100, (kbps / 320) * 100)}%`;
        const tier = tierForKbps(kbps);
        if (tierEl.textContent !== tier.label) {
          tierEl.textContent = tier.label;
          tierEl.className = `rounded px-1.5 py-0.5 ${tier.cls}`;
        }
      },
      onComplete: () => {
        gsap.delayedCall(1.2, step);
      },
    });
  };
  gsap.delayedCall(1.6, step);
}

/* ---------- Stat counters (GSAP + ScrollTrigger) ---------- */
for (const el of document.querySelectorAll<HTMLElement>("[data-count]")) {
  const target = Number(el.dataset.count);
  const state = { n: 0 };
  ScrollTrigger.create({
    trigger: el,
    start: "top 85%",
    once: true,
    onEnter: () => {
      gsap.to(state, {
        n: target,
        duration: reducedMotion ? 0 : 1.4,
        ease: "power2.out",
        onUpdate: () => {
          el.textContent = String(Math.round(state.n));
        },
      });
    },
  });
}

/* ---------- Tier bars fill on scroll (GSAP scrub) ---------- */
for (const row of document.querySelectorAll<HTMLElement>(".tier-row")) {
  const fill = row.querySelector<HTMLElement>(".tier-fill");
  if (!fill) continue;
  gsap.to(fill, {
    width: `${row.dataset.width}%`,
    ease: "none",
    scrollTrigger: {
      trigger: row,
      start: "top 90%",
      end: "top 55%",
      scrub: reducedMotion ? false : 0.6,
    },
  });
}

/* ---------- Tutorial steps: alternating slide-in (GSAP) ---------- */
document.querySelectorAll<HTMLElement>("[data-step]").forEach((step, i) => {
  const fromX = i % 2 === 0 ? -32 : 32;
  gsap.from(step, {
    x: reducedMotion ? 0 : fromX,
    autoAlpha: 0,
    duration: 0.9,
    ease: "power3.out",
    scrollTrigger: { trigger: step, start: "top 80%", once: true },
  });
});

/* ---------- Card reveals (motion.dev inView) ---------- */
inView(
  "[data-reveal]",
  (el) => {
    animate(
      el,
      { opacity: [0, 1], transform: ["translateY(24px)", "translateY(0px)"] },
      { duration: reducedMotion ? 0 : 0.6, ease: [0.22, 1, 0.36, 1] },
    );
  },
  { margin: "0px 0px -12% 0px" },
);

/* ---------- Button micro-interactions (motion.dev springs) ---------- */
if (!reducedMotion) {
  hover(".cta-btn", (el) => {
    animate(el, { scale: 1.04 }, { type: "spring", stiffness: 500, damping: 24 });
    return () => animate(el, { scale: 1 }, { type: "spring", stiffness: 500, damping: 24 });
  });
  press(".cta-btn", (el) => {
    animate(el, { scale: 0.96 }, { type: "spring", stiffness: 700, damping: 30 });
    return () => animate(el, { scale: 1.04 }, { type: "spring", stiffness: 500, damping: 24 });
  });
}

/* ---------- Nav: shadow once scrolled ---------- */
const nav = document.getElementById("nav");
if (nav) {
  ScrollTrigger.create({
    start: 24,
    onUpdate: (self) => {
      nav.classList.toggle("shadow-lg", self.scroll() > 24);
      nav.classList.toggle("shadow-black/40", self.scroll() > 24);
    },
  });
}
