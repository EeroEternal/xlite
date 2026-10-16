# Admin domain abstractions

xlite Admin has **four domains plus authentication**, and that is the whole set. The scope rule in [`design.md`](../design.md#scope) says the same thing from the UI side: there is no organization, project, route, user/role, quota, billing, routing-strategy, or plugin domain, and no demo mode. Do not add types, resources, endpoints, nav entries, or copy for them; a new capability starts in [`architecture.md`](../architecture.md).

Keep the page → hook/resource → api-types → handler direction.

| Domain | Types | Resources | Page |
| --- | --- | --- | --- |
| Providers | `api-types/providers.ts` | `resources/providers.ts` | `pages/models.tsx`（供应商随模型实例创建） |
| Models / channels | `api-types/models.ts` | `resources/models.ts` | `pages/models.tsx` |
| API keys | `api-types/api-keys.ts` | `resources/api-keys.ts` | `pages/api-keys.tsx` |
| Overview / settings | `api-types/overview.ts`, `api-types/catalog.ts` | `resources/overview.ts`, `resources/catalog.ts` | `pages/overview.tsx`, `pages/settings.tsx` |

Authentication is a cross-cutting session, not a domain: `lib/auth.tsx` + `resources/auth.ts`, backed by `src/auth.rs`.

## Rules

- List/detail DTOs never include a full API key. Create and rotate responses may include `key` once.
- Resource functions own URLs. Pages do not concatenate `/api/...`.
- The Admin REST surface is exactly the CRUD routes for those four domains in `src/admin.rs`, plus `/api/auth/*` and `/api/catalog/{export,import}`. There are no read-one (`GET .../{id}`) endpoints; a list plus a client-side lookup is enough at this scale.
- Adding a domain means adding a page, a nav entry, and typed resources together — never a page that reaches into another domain's URLs.
