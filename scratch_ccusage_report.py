#!/usr/bin/env python3
"""
scratch_ccusage_report.py - analyze ccusage daily JSON and print a report.

Usage:
    ccusage daily --json --offline | python scratch_ccusage_report.py
    python scratch_ccusage_report.py path/to/ccusage.json
    python scratch_ccusage_report.py           # defaults to scratch_ccusage.json

Reports:
    - Daily metrics with cache amplification ratios
    - Totals, distribution, weekly trend
    - Top expensive days, outliers, per-model breakdown
    - Headline findings (early window vs late window)

Key ratios:
    cr/out    cache_read tokens per output token. High value means a lot of
              cached context is being re-billed per token generated. Sensitive
              to both session shape (context size, turns per response) and
              backend cache behavior.
    cr/create cache_read per cache_create. Rough proxy for how many times
              each written cache token gets re-read. Spikes here point at
              read-side amplification.
    $/1k out  dollars per thousand output tokens. Whole-bill efficiency.
"""

import json
import os
import sys
from datetime import datetime
from statistics import mean, median

DEFAULT_PATH = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'scratch_ccusage.json')


def load_data():
    if len(sys.argv) > 1:
        with open(sys.argv[1]) as f:
            return json.load(f)
    if not sys.stdin.isatty():
        return json.load(sys.stdin)
    with open(DEFAULT_PATH) as f:
        return json.load(f)


def ratio(num, den):
    return num / den if den else 0.0


def fmt_money(n):
    return f"${n:,.2f}"


def daily_metrics(day):
    out = day['outputTokens']
    cr = day['cacheReadTokens']
    cc = day['cacheCreationTokens']
    inp = day['inputTokens']
    cost = day['totalCost']
    return {
        'date': day['date'],
        'input': inp,
        'output': out,
        'cache_create': cc,
        'cache_read': cr,
        'total_tokens': day['totalTokens'],
        'cost': cost,
        'cr_per_out': ratio(cr, out),
        'cr_per_cc': ratio(cr, cc),
        'cost_per_kout': ratio(cost, out / 1000.0),
        'models': day.get('modelsUsed', []),
    }


def print_header(title):
    bar = '=' * 94
    print()
    print(bar)
    print(title)
    print(bar)


def print_section(title):
    print()
    print(f"== {title} ==")


def print_daily_table(rows):
    print_section(f"Daily ({len(rows)} days)")
    header = (
        f"{'Date':<12}{'Input':>9}{'Output':>12}{'CacheCre':>14}"
        f"{'CacheRead':>18}{'cr/out':>10}{'cr/cc':>9}{'$/kOut':>11}{'Cost':>12}"
    )
    print(header)
    print('-' * len(header))
    for r in rows:
        print(
            f"{r['date']:<12}"
            f"{r['input']:>9,}"
            f"{r['output']:>12,}"
            f"{r['cache_create']:>14,}"
            f"{r['cache_read']:>18,}"
            f"{r['cr_per_out']:>9,.0f}x"
            f"{r['cr_per_cc']:>8,.0f}x"
            f"${r['cost_per_kout']:>9.3f}"
            f"${r['cost']:>11,.2f}"
        )


def aggregate(rows):
    total_out = sum(r['output'] for r in rows)
    total_cc = sum(r['cache_create'] for r in rows)
    total_cr = sum(r['cache_read'] for r in rows)
    total_cost = sum(r['cost'] for r in rows)
    return {
        'days': len(rows),
        'input': sum(r['input'] for r in rows),
        'output': total_out,
        'cache_create': total_cc,
        'cache_read': total_cr,
        'cost': total_cost,
        'cr_per_out': ratio(total_cr, total_out),
        'cr_per_cc': ratio(total_cr, total_cc),
        'cost_per_kout': ratio(total_cost, total_out / 1000.0),
    }


def print_totals(total):
    print_section(f"Totals over {total['days']} days")
    lines = [
        ('Input tokens',        f"{total['input']:>22,}"),
        ('Output tokens',       f"{total['output']:>22,}"),
        ('Cache create tokens', f"{total['cache_create']:>22,}"),
        ('Cache read tokens',   f"{total['cache_read']:>22,}"),
        ('Total cost',          f"{fmt_money(total['cost']):>22}"),
        ('cr/out ratio',        f"{total['cr_per_out']:>21,.0f}x"),
        ('cr/create ratio',     f"{total['cr_per_cc']:>21,.0f}x"),
        ('$/1k output',         f"{fmt_money(total['cost_per_kout']):>22}"),
    ]
    for label, val in lines:
        print(f"  {label:<22}{val}")


def print_distribution(rows):
    valid = [r for r in rows if r['output'] > 0]
    cr_out = [r['cr_per_out'] for r in valid]
    cr_cc = [r['cr_per_cc'] for r in valid if r['cache_create'] > 0]
    cost_out = [r['cost_per_kout'] for r in valid]
    daily_cost = [r['cost'] for r in rows]

    print_section("Daily distribution")
    header = f"{'metric':<18}{'min':>14}{'median':>14}{'mean':>14}{'max':>14}"
    print(header)
    print('-' * len(header))

    def row(label, data, fmt):
        if not data:
            return
        vals = [fmt(min(data)), fmt(median(data)), fmt(mean(data)), fmt(max(data))]
        print(f"{label:<18}" + ''.join(f"{v:>14}" for v in vals))

    row('cr/out',       cr_out,     lambda v: f"{v:,.0f}x")
    row('cr/create',    cr_cc,      lambda v: f"{v:,.0f}x")
    row('$/1k output',  cost_out,   lambda v: f"${v:.3f}")
    row('daily cost',   daily_cost, lambda v: f"${v:,.2f}")


def print_weekly_trend(rows):
    print_section("Weekly trend")
    buckets = {}
    for r in sorted(rows, key=lambda r: r['date']):
        y, w, _ = datetime.fromisoformat(r['date']).isocalendar()
        key = f"{y}-W{w:02d}"
        b = buckets.setdefault(key, {'out': 0, 'cr': 0, 'cc': 0, 'cost': 0, 'days': 0})
        b['out'] += r['output']
        b['cr'] += r['cache_read']
        b['cc'] += r['cache_create']
        b['cost'] += r['cost']
        b['days'] += 1
    header = (
        f"{'Week':<10}{'Days':>6}{'Output':>14}{'CacheRead':>18}"
        f"{'cr/out':>11}{'cr/cc':>10}{'$/kOut':>11}{'Cost':>12}"
    )
    print(header)
    print('-' * len(header))
    for key in sorted(buckets):
        b = buckets[key]
        cr_out = ratio(b['cr'], b['out'])
        cr_cc = ratio(b['cr'], b['cc'])
        cost_out = ratio(b['cost'], b['out'] / 1000.0)
        print(
            f"{key:<10}{b['days']:>6}{b['out']:>14,}{b['cr']:>18,}"
            f"{cr_out:>10,.0f}x{cr_cc:>9,.0f}x${cost_out:>9.3f}${b['cost']:>11,.2f}"
        )


def print_top_days(rows, n=10):
    print_section(f"Top {n} most expensive days")
    top = sorted(rows, key=lambda r: -r['cost'])[:n]
    header = (
        f"{'Date':<12}{'Cost':>12}{'Output':>14}{'CacheRead':>18}"
        f"{'cr/out':>10}{'cr/cc':>9}{'$/kOut':>11}"
    )
    print(header)
    print('-' * len(header))
    for r in top:
        print(
            f"{r['date']:<12}${r['cost']:>10,.2f}{r['output']:>14,}"
            f"{r['cache_read']:>18,}{r['cr_per_out']:>9,.0f}x"
            f"{r['cr_per_cc']:>8,.0f}x${r['cost_per_kout']:>9.3f}"
        )


def print_outliers(rows, total):
    print_section("Outliers ($/kOut >= 2x overall average, cost >= $10)")
    threshold = total['cost_per_kout'] * 2
    outliers = [r for r in rows if r['cost_per_kout'] >= threshold and r['cost'] >= 10]
    if not outliers:
        print(f"  None. Threshold was ${threshold:.3f}/kOut.")
        return
    outliers.sort(key=lambda r: -r['cost_per_kout'])
    header = (
        f"{'Date':<12}{'Cost':>12}{'$/kOut':>11}{'cr/out':>10}{'cr/cc':>9}{'Output':>13}"
    )
    print(header)
    print('-' * len(header))
    for r in outliers[:15]:
        print(
            f"{r['date']:<12}${r['cost']:>10,.2f}${r['cost_per_kout']:>9.3f}"
            f"{r['cr_per_out']:>9,.0f}x{r['cr_per_cc']:>8,.0f}x{r['output']:>13,}"
        )


def print_model_breakdown(data):
    print_section("Per-model totals")
    totals = {}
    for day in data['daily']:
        for m in day.get('modelBreakdowns', []):
            name = m.get('modelName', 'unknown')
            b = totals.setdefault(name, {'in': 0, 'out': 0, 'cc': 0, 'cr': 0, 'cost': 0})
            b['in'] += m.get('inputTokens', 0)
            b['out'] += m.get('outputTokens', 0)
            b['cc'] += m.get('cacheCreationTokens', 0)
            b['cr'] += m.get('cacheReadTokens', 0)
            b['cost'] += m.get('cost', 0)
    if not totals:
        print("  No modelBreakdowns present in input.")
        return
    header = (
        f"{'Model':<32}{'Cost':>12}{'Share':>9}{'Input':>12}"
        f"{'Output':>14}{'CacheRead':>18}"
    )
    print(header)
    print('-' * len(header))
    total_cost = sum(b['cost'] for b in totals.values())
    for name, b in sorted(totals.items(), key=lambda kv: -kv[1]['cost']):
        share = 100 * b['cost'] / total_cost if total_cost else 0
        print(
            f"{name:<32}${b['cost']:>10,.2f}{share:>8.1f}%{b['in']:>12,}"
            f"{b['out']:>14,}{b['cr']:>18,}"
        )


def print_findings(rows, total):
    print_section("Headline findings")
    rows_sorted = sorted(rows, key=lambda r: r['date'])
    n = len(rows_sorted)
    q = max(1, n // 4)
    early = rows_sorted[:q]
    late = rows_sorted[-q:]

    def agg(batch):
        out = sum(r['output'] for r in batch)
        cr = sum(r['cache_read'] for r in batch)
        cc = sum(r['cache_create'] for r in batch)
        cost = sum(r['cost'] for r in batch)
        return {
            'start': batch[0]['date'], 'end': batch[-1]['date'],
            'cr_per_out': ratio(cr, out),
            'cr_per_cc': ratio(cr, cc),
            'cost_per_kout': ratio(cost, out / 1000.0),
            'cost': cost, 'output': out,
        }

    e = agg(early)
    l = agg(late)
    print(f"  Date range:        {rows_sorted[0]['date']} to {rows_sorted[-1]['date']} ({n} days)")
    print(f"  Total spent:       {fmt_money(total['cost'])}")
    print(f"  Overall $/kOut:    ${total['cost_per_kout']:.3f}")
    print(f"  Overall cr/out:    {total['cr_per_out']:,.0f}x")
    print()
    print(f"  First {q} days window ({e['start']} to {e['end']}):")
    print(f"    cr/out:          {e['cr_per_out']:>10,.0f}x")
    print(f"    cr/create:       {e['cr_per_cc']:>10,.0f}x")
    print(f"    $/1k output:     ${e['cost_per_kout']:>9.3f}")
    print(f"    total cost:      {fmt_money(e['cost']):>11}")
    print()
    print(f"  Last {q} days window ({l['start']} to {l['end']}):")
    print(f"    cr/out:          {l['cr_per_out']:>10,.0f}x")
    print(f"    cr/create:       {l['cr_per_cc']:>10,.0f}x")
    print(f"    $/1k output:     ${l['cost_per_kout']:>9.3f}")
    print(f"    total cost:      {fmt_money(l['cost']):>11}")
    print()

    def delta(label, e_val, l_val, improvement_is_down=True):
        if not e_val or not l_val:
            return
        if improvement_is_down:
            factor = e_val / l_val
            direction = 'improvement' if factor > 1 else 'regression'
            print(f"  {label:<18}{factor:>6.2f}x ({direction}: early / late)")
        else:
            factor = l_val / e_val
            direction = 'increase' if factor > 1 else 'decrease'
            print(f"  {label:<18}{factor:>6.2f}x ({direction}: late / early)")

    delta('cr/out change:',     e['cr_per_out'],     l['cr_per_out'])
    delta('cr/create change:',  e['cr_per_cc'],      l['cr_per_cc'])
    delta('$/kOut change:',     e['cost_per_kout'],  l['cost_per_kout'])


def main():
    data = load_data()
    rows = [daily_metrics(d) for d in data['daily']]
    rows.sort(key=lambda r: r['date'])

    print_header("ccusage report")
    print_daily_table(rows)
    total = aggregate(rows)
    print_totals(total)
    print_distribution(rows)
    print_weekly_trend(rows)
    print_top_days(rows)
    print_outliers(rows, total)
    print_model_breakdown(data)
    print_findings(rows, total)
    print()


if __name__ == '__main__':
    main()
