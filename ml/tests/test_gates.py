from diogenes_phase0.gates import all_passed, check_gates, render_markdown

GATE_CFG = {"key_exact_min": 0.85, "mouse_r2_min": 0.9, "macro_f1_warn": 0.6}


def _report(key_exact, r2):
    return {"n": 100, "key_exact": key_exact, "key_macro_f1": 0.7,
            "per_key_f1": {"87": 0.9, "49": 0.3},
            "bin_acc_x": 0.8, "bin_acc_y": 0.8,
            "mouse_r2_x": r2, "mouse_r2_y": r2 + 0.01, "mouse_r2_min": r2}


def test_pass():
    gates = check_gates({"val": _report(0.9, 0.95)}, GATE_CFG)
    assert all_passed(gates)


def test_fail_on_key_exact():
    gates = check_gates({"val": _report(0.7, 0.95)}, GATE_CFG)
    assert not all_passed(gates)


def test_fail_on_r2():
    gates = check_gates({"val": _report(0.9, 0.5)}, GATE_CFG)
    assert not all_passed(gates)


def test_zero_shot_is_informational():
    reports = {"val": _report(0.9, 0.95),
               "zero_shot": {**_report(0.1, -0.5), "n": 50}}
    gates = check_gates(reports, GATE_CFG)
    assert all_passed(gates)  # zero-shot never blocks — it's the early signal
    md = render_markdown(gates, reports)
    assert "zero_shot.key_exact" in md and "PASS" in md


def test_learning_signal_floor_gates():
    cfg = {"key_macro_f1_min": 0.22, "bin_acc_min": 0.25}
    r = _report(0.1, -0.5)  # bad accuracy metrics are NOT gated here
    gates = check_gates({"val": {**r, "key_macro_f1": 0.3,
                                 "bin_acc_x": 0.3, "bin_acc_y": 0.27}}, cfg)
    assert all_passed(gates)
    gates = check_gates({"val": {**r, "key_macro_f1": 0.3,
                                 "bin_acc_x": 0.3, "bin_acc_y": 0.1}}, cfg)
    assert not all_passed(gates)  # min over axes fails the floor


def test_missing_metric_does_not_pass_silently():
    gates = check_gates({"val": {"n": 10, "key_exact": None,
                                 "mouse_r2_min": None}}, GATE_CFG)
    hard = [g for g in gates if g.level == "hard"]
    assert all(g.passed is None for g in hard)
    assert all_passed(gates)  # vacuous pass is allowed but visible as n/a in the report
