# xlite A–F

COMPLETE A-F GROUP COVERAGE / TWO DOCUMENTED EXECUTION BATCHES

Group configurations: `{"A": {"cpu_quotas": {"farm": 28, "gateway": 28, "k6": 56}, "k6_memory_limit": null, "memory_gib": {"farm": 64, "gateway": 64, "k6": 192}, "source": "../sources/r10/results"}, "B": {"cpu_quotas": {"farm": 28, "gateway": 28, "k6": 56}, "k6_memory_limit": null, "memory_gib": {"farm": 64, "gateway": 64, "k6": 192}, "source": "../sources/r10/results"}, "C": {"cpu_quotas": {"farm": 28, "gateway": 28, "k6": 56}, "k6_memory_limit": null, "memory_gib": {"farm": 64, "gateway": 64, "k6": 192}, "source": "../sources/r10/results"}, "D": {"cpu_quotas": {"farm": 28, "gateway": 28, "k6": 56}, "k6_memory_limit": "9GiB", "memory_gib": {"farm": 64, "gateway": 64, "k6": 192}, "source": "../sources/r11/results"}, "E": {"cpu_quotas": {"farm": 28, "gateway": 28, "k6": 56}, "k6_memory_limit": null, "memory_gib": {"farm": 64, "gateway": 64, "k6": 192}, "source": "../sources/r10/results"}, "F": {"cpu_quotas": {"farm": 28, "gateway": 28, "k6": 56}, "k6_memory_limit": "9GiB", "memory_gib": {"farm": 64, "gateway": 64, "k6": 192}, "source": "../sources/r11/results"}}`
A/B/C/E: default Go soft limit. D/F: GOMEMLIMIT=9GiB; all hard budgets unchanged.
r10 D direct preflight hit client OOM. No within-group load curve mixes settings.

batch=1, TTFT500ms, sample=0.5. TTFT columns are 16-shard ranges, not global percentiles.
Pinned xlite ad4e653 / 0.5.3; not a measurement of latest xlite.
Private internal Docker bridge; xlite upstream h2c, private SQLite frozen catalog.
New API was not tested in this independent batch.

| Gateway | Group | VU | Valid | Whole QPS | Steady success QPS | Activity | TTFT P50 range ms | P99 range ms |
|---|---|---:|---|---:|---:|---:|---:|---:|
| xlite | A | 1000 | True | 173.633 | 183.240 | 999.543 | 504.443–504.790 | 520.392–522.275 |
| xlite | A | 2000 | True | 347.283 | 366.467 | 1999.066 | 503.449–503.535 | 509.544–510.324 |
| xlite | A | 5000 | True | 868.308 | 916.363 | 4997.524 | 502.534–502.574 | 505.244–505.553 |
| xlite | A | 10000 | True | 1736.442 | 1832.640 | 9994.736 | 502.390–502.411 | 505.270–505.633 |
| xlite | A | 15000 | True | 2604.461 | 2748.803 | 14991.783 | 502.370–502.402 | 506.086–506.427 |
| xlite | A | 18000 | True | 3125.254 | 3298.397 | 17989.836 | 502.367–502.394 | 508.348–509.521 |
| xlite | B | 1000 | True | 172.084 | 181.557 | 998.427 | 502.678–502.736 | 505.252–505.606 |
| xlite | B | 2000 | True | 344.140 | 363.153 | 1996.641 | 502.001–502.044 | 503.870–504.106 |
| xlite | B | 5000 | True | 860.165 | 907.763 | 4990.834 | 501.949–501.989 | 504.154–504.616 |
| xlite | B | 10000 | True | 1720.600 | 1815.990 | 9984.605 | 502.023–502.058 | 513.055–515.840 |
| xlite | B | 15000 | True | 2566.088 | 2707.897 | 14976.257 | 518.330–518.515 | 569.263–589.599 |
| xlite | B | 18000 | True | 3053.312 | 3220.950 | 17968.799 | 522.076–522.411 | 607.343–682.922 |
| xlite | C | 1000 | True | 15.288 | 15.881 | 999.695 | 504.871–505.408 | 515.231–520.934 |
| xlite | C | 2000 | True | 30.569 | 31.759 | 1999.376 | 503.560–503.748 | 510.662–515.099 |
| xlite | C | 5000 | True | 76.402 | 79.383 | 4998.395 | 502.659–502.728 | 506.725–508.478 |
| xlite | C | 10000 | True | 152.782 | 158.763 | 9996.648 | 502.470–502.522 | 506.358–509.428 |
| xlite | C | 15000 | True | 229.164 | 238.130 | 14994.816 | 502.470–502.499 | 508.656–511.791 |
| xlite | C | 18000 | True | 274.993 | 285.756 | 17993.742 | 502.467–502.498 | 513.315–515.899 |
| xlite | D | 1000 | True | 15.246 | 15.837 | 998.797 | 502.768–502.861 | 505.630–508.769 |
| xlite | D | 2000 | True | 30.476 | 31.659 | 1997.606 | 502.078–502.183 | 504.585–507.179 |
| xlite | D | 5000 | True | 76.143 | 79.119 | 4993.835 | 502.053–502.117 | 505.568–508.912 |
| xlite | D | 10000 | True | 152.239 | 158.192 | 9986.948 | 506.837–507.767 | 560.650–570.644 |
| xlite | D | 15000 | True | 227.701 | 236.578 | 14969.526 | 523.683–528.679 | 632.249–684.997 |
| xlite | D | 18000 | True | 272.346 | 282.867 | 17917.238 | 553.956–580.838 | 724.454–980.532 |
| xlite | E | 1000 | True | 172.552 | 182.090 | 999.146 | 503.250–503.357 | 508.167–509.075 |
| xlite | E | 2000 | True | 345.106 | 364.187 | 1998.218 | 502.631–502.667 | 505.309–505.638 |
| xlite | E | 5000 | True | 862.680 | 910.453 | 4995.133 | 501.843–501.874 | 503.675–503.799 |
| xlite | E | 10000 | True | 1725.012 | 1820.603 | 9989.467 | 502.011–502.040 | 504.805–505.669 |
| xlite | E | 15000 | True | 2587.481 | 2731.080 | 14984.847 | 502.084–502.109 | 510.256–512.187 |
| xlite | E | 18000 | True | 3105.114 | 3277.360 | 17983.803 | 502.123–502.154 | 515.729–519.314 |
| xlite | F | 1000 | True | 15.262 | 15.855 | 999.329 | 503.303–503.521 | 508.868–512.183 |
| xlite | F | 2000 | True | 30.515 | 31.705 | 1998.631 | 502.677–502.805 | 505.961–508.950 |
| xlite | F | 5000 | True | 76.265 | 79.245 | 4996.517 | 501.932–501.986 | 504.324–505.761 |
| xlite | F | 10000 | True | 152.516 | 158.475 | 9992.934 | 502.111–502.156 | 510.432–517.093 |
| xlite | F | 15000 | True | 228.777 | 237.728 | 14990.477 | 502.183–502.247 | 528.763–547.275 |
| xlite | F | 18000 | True | 274.348 | 285.078 | 17988.526 | 514.023–514.578 | 579.456–605.608 |
