# Lambo PHP website

The landing page for Lambo PHP - a **zero-build static site**. No npm, no
bundler, no framework: `index.html` + `styles.css` + `main.js` is the whole
thing. This is a deliberate tradeoff (a JS toolchain in a Rust repo is
maintenance tax for one page); if the site ever grows into docs hosting,
these semantic sections port cleanly to Astro.

## Preview locally

Just open `index.html` in a browser, or serve the directory:

```bash
python3 -m http.server -d web 8000
# → http://localhost:8000
```

## Editing guide

| What | Where |
| --- | --- |
| Copy, sections, tables | `index.html` |
| Colors, spacing, layout | `styles.css` (design tokens at the top of `:root`) |
| Terminal demo script | `main.js` → the `SCRIPT` array (keep it faithful to real CLI output!) |
| Copy-button, reveal behavior | `main.js` |
| Logo/favicon | `favicon.svg` |

Rules that keep the page honest and fast:

- **The terminal demo shows real CLI output.** When a command's output
  changes, update `SCRIPT` in the same PR - the demo is documentation.
- **No fabricated benchmarks.** The numbers section displays *design
  budgets* from `docs/architecture.md` and says so. Measured numbers may
  replace them once benchmarks land in CI (M7).
- **No third-party assets.** No web fonts, no trackers, no CDN: everything
  served from this directory. Dark-only palette is a deliberate aesthetic.
- **Accessibility is not optional:** semantic landmarks, `aria` labels on
  the terminal and copy buttons, `prefers-reduced-motion` support.

## Deployment

`.github/workflows/pages.yml` publishes this directory to **GitHub Pages**
on every push to `main` that touches `web/`. Enable it once:
*Settings → Pages → Build and deployment → GitHub Actions*.

The canonical URL is <https://flessan.github.io/lambophp/>.
