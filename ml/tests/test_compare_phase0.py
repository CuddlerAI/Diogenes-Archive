"""The comparison table must read the same metric across every column.

The mistake it exists to prevent is quoting one run's lenient reading
against another run's strict one. Rows are keyed by metric name, so a run
that is missing a metric has to show a gap rather than borrow a neighbour's
number.
"""
import json

import pytest

from diogenes_phase0.compare_phase0 import load_run, render


def _report(tmp_path, name, **metrics):
    d = tmp_path / name
    d.mkdir()
    (d / "gates_report.json").write_text(json.dumps({
        "gates": [], "all_hard_passed": metrics.pop("passed", True),
        "reports": {"val": metrics},
    }))
    return d


def test_missing_metric_shows_a_gap_not_a_neighbour_value(tmp_path):
    a = _report(tmp_path, "a", key_exact=0.5, key_macro_f1=0.4, n=100)
    b = _report(tmp_path, "b", key_exact=0.7, n=200)   # no macro F1
    table = render({"a": load_run(a), "b": load_run(b)})
    row = [ln for ln in table.splitlines() if "macro F1 (gated)" in ln][0]
    assert "0.4000" in row and "—" in row
    assert "0.7000" not in row


def test_row_is_dropped_when_no_run_has_it(tmp_path):
    a = _report(tmp_path, "a", key_exact=0.5, n=10)
    table = render({"a": load_run(a)})
    assert "mouse R^2" not in table
    assert "key exact-set" in table


def test_gate_verdict_is_per_run(tmp_path):
    a = _report(tmp_path, "a", key_exact=0.9, n=10, passed=True)
    b = _report(tmp_path, "b", key_exact=0.2, n=10, passed=False)
    table = render({"a": load_run(a), "b": load_run(b)})
    row = [ln for ln in table.splitlines() if "hard gates" in ln][0]
    assert "PASS" in row and "FAIL" in row
    assert row.index("PASS") < row.index("FAIL")


def test_reads_eval_checkpoint_reports_too(tmp_path):
    """Cross-game transfer results come from eval_checkpoint, not train."""
    p = tmp_path / "cross.json"
    p.write_text(json.dumps({
        "label": "mc-head-on-valorant",
        "metrics": {"key_exact": 0.31, "key_macro_f1": 0.12, "n": 500},
    }))
    run = load_run(p)
    assert run["metrics"]["key_exact"] == 0.31
    assert run["passed"] is None
    table = render({"mc->val": run})
    assert "0.3100" in table
    # no gate verdict for an eval-only report
    assert "hard gates" not in table


def test_missing_report_is_an_error_not_an_empty_column(tmp_path):
    d = tmp_path / "empty"
    d.mkdir()
    with pytest.raises(SystemExit):
        load_run(d)
