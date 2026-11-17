# Build the share-link viewer

The share-link viewer is the page a recipient opens at `/s/<link_id>`. Its source is the web package `server/web/`, and the build writes `server/web/dist/`, which the Worker serves as static assets. [RFC 0005 §8](../spec/rfc/0005-share-links.md#8-web-viewer) defines what the page shows and the headers the Worker sends with it.

## Build and check the viewer

Run these in `server/`:

1. Install the dependencies with `pnpm install`. `server/web/` is a workspace package of `server/`, named `koidrive-web`.
2. Build the viewer with `pnpm build:web`. The output goes to `server/web/dist/`, which is not committed.
3. Type-check it with `pnpm typecheck`, which also checks the server.

You rarely run step 2 by hand. `wrangler.jsonc` names `pnpm build:web` as its `build.command`, so `wrangler dev`, `wrangler deploy` and the dry run that packs `koi`'s server bundle build the viewer first. `pnpm test` builds it before the tests run.

## Keep the build inside the CSP

The viewer's CSP allows script and style only from the page's own origin and forbids inline code. `server/web/vite.config.ts` keeps the build within it:

- `modulePreload: { polyfill: false }` and `assetsInlineLimit: 0`, so the build inlines no script and turns no asset into a `data:` URL.
- Output names without a hash (`[name]/[name].js`), so the URLs stay `/viewer/viewer.js` and `/viewer/viewer.css`.
- No UI framework. Vite and TypeScript are build-time dependencies only, so the built page holds no third-party code.

The page renders every name with `textContent`, never as HTML.

## Change the headers in both places

`server/web/public/_headers` sets the headers of the assets under `/viewer/`, and the build copies it into `dist/`. `server/src/share/routes.ts` sets the same headers on `/s/<link_id>`, which the Worker serves itself. When you change one, make the same change to the other.

## Add a page

Add the page's HTML entry to `rolldownOptions.input` in `server/web/vite.config.ts`. Each entry builds to its own directory under `dist/`. Keep the package inside `server/`, because [`server/` builds on its own](design/deploy-and-upgrade.md#1-server-builds-on-its-own).
