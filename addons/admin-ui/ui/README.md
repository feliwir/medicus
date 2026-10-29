# medicus admin UI

The administration and viewing interface (ADR-0004): React, TypeScript, Vite, Tailwind CSS v4 with
shadcn/ui-style components (`src/components/ui`, ours to edit), TanStack Router and Query.

```sh
npm ci
npm run dev        # http://localhost:5173, forwarding /api, /rs, /nodes to a server on :8042
npm run typecheck
npm test
npm run build      # dist/, embedded by the admin-ui addon on the next `cargo build`
```

`MEDICUS_URL` points the development server at another medicus server. The production bundle is
served by the `admin-ui` addon; a server built before `npm run build` serves a placeholder page
instead (`MEDICUS_UI_DIST` embeds a bundle from elsewhere).

- `src/index.css`: the design tokens (colours, radius, fonts) for light and dark mode.
- `src/lib/api.ts`: the admin API client and its types.
- `src/routes/`: the screens; `src/router.tsx` maps them to paths.
