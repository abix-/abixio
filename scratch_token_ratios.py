import json, sys

with open(r'C:\code\abixio\scratch_ccusage.json') as f:
    data = json.load(f)

rows = data['daily']

print(f"{'Date':<12} {'I+O':>14} {'CW+CR':>18} {'Pct':>10} {'Cost':>10}")
print("-" * 68)
tI=tO=tCW=tCR=tCost=0
for r in rows:
    d = r['date']
    i = r['inputTokens']
    o = r['outputTokens']
    cw = r['cacheCreationTokens']
    cr = r['cacheReadTokens']
    cost = r['totalCost']
    io = i+o
    c = cw+cr
    pct = 100*io/c if c else float('inf')
    print(f"{d:<12} {io:>14,} {c:>18,} {pct:>9.4f}% {cost:>9.2f}")
    tI+=i; tO+=o; tCW+=cw; tCR+=cr; tCost+=cost

io = tI+tO
c = tCW+tCR
pct = 100*io/c
print("-" * 68)
print(f"{'TOTAL':<12} {io:>14,} {c:>18,} {pct:>9.4f}% {tCost:>9.2f}")
print()
print(f"Days: {len(rows)}")
print(f"Input total:       {tI:>18,}")
print(f"Output total:      {tO:>18,}")
print(f"Cache write total: {tCW:>18,}")
print(f"Cache read total:  {tCR:>18,}")
print(f"Total cost (USD):  ${tCost:>17,.2f}")
print()
# summary stats on daily pct
pcts = []
for r in rows:
    io=r['inputTokens']+r['outputTokens']
    c=r['cacheCreationTokens']+r['cacheReadTokens']
    if c: pcts.append(100*io/c)
pcts.sort()
n=len(pcts)
median = pcts[n//2] if n%2 else (pcts[n//2-1]+pcts[n//2])/2
print(f"Daily pct  min: {min(pcts):.4f}%  median: {median:.4f}%  max: {max(pcts):.4f}%")
