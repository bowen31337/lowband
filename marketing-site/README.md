# LowBand marketing page

A single-page marketing site promoting LowBand — the P2P remote-assist and
conferencing tool for constrained networks — including a four-step usage
tutorial, feature tour, tier explainer, and comparison against mainstream
conferencing tools.

## Stack

- [Vite](https://vitejs.dev) — dev server & build
- [Tailwind CSS v4](https://tailwindcss.com) — styling (via `@tailwindcss/vite`)
- [Biome](https://biomejs.dev) — lint & format
- [Motion](https://motion.dev) — in-view reveals and spring micro-interactions
- [GSAP](https://gsap.com) + ScrollTrigger — hero timeline, scroll-scrubbed tier
  bars, stat counters, and the live link simulation in the hero terminal

## Commands

```sh
npm install
npm run dev       # dev server
npm run build     # production build to dist/
npm run preview   # serve the production build
npm run lint      # biome check
npm run lint:fix  # biome check --write
```
