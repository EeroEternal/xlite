# Admin domain abstractions

xlite Admin has four domains. Keep the page → hook/resource → api-types → handler direction.

| Domain | Types | Resources | Page |
| --- | --- | --- | --- |
| Providers | `api-types/providers.ts` | `resources/providers.ts` | `pages/providers.tsx` |
| Models / channels | `api-types/models.ts` | `resources/models.ts` | `pages/models.tsx` |
| API keys | `api-types/api-keys.ts` | `resources/api-keys.ts` | `pages/api-keys.tsx` |
| Overview / settings | `api-types/overview.ts` | `resources/overview.ts` | `pages/overview.tsx`, `pages/settings.tsx` |

- List/detail DTOs never include a full API key. Create and rotate responses may include `key` once.
- Resource functions own URLs. Pages do not concatenate `/api/...`.
- There is no organization, project, routing-strategy, billing, or plugin domain.
