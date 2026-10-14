"""调度器 enabled 过滤（D1）：手动路径都跳过禁用账号，自动排程曾经不跳。

`wb_proxy.py` 的手动入口（`:16064`、`:17246/17285/17311`、`:17456/17480`）和
`wb_accounts.py:2315` 都写了 `a.enabled`，唯独 `wb_scheduler.py:144` 的
`for acc in list(self.pool.accounts):` 是裸遍历。结果是「面板里禁用了账号，
自动排程照样拿它去刷 token / 签到 / 打卡」——用户以为关了，实际还在发请求，
既浪费额度也平白吃风控。

这里钉住该过滤存在且生效：

  1. 禁用账号在自动巡检里不被触碰（refresh / checkin / report 均不调用）；
  2. 启用账号照常执行（防止把过滤写成「全都不跑」）；
  3. intl 禁用账号的每日活跃打卡同样被跳过；
  4. 空账号早退分支不受影响。

`do_cat_travel` / `run_streak_bonus` 会打真实网络（wb_tasks 的 urllib 路径），
与本套件关注的过滤无关，统一旁路成记账桩，保证全程零请求、零凭证。

Run with the current interpreter (python tests/run_all.py scheduler_enabled).
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import wb_scheduler

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def make_account(uid, realm="cn", enabled=True):
    """造一个只带调度所需字段的账号桩，把业务方法全部替换成计数器。

    不构造真正的 wb_accounts.Account —— 那会牵出 token 刷新与网络路径。
    调度器只碰 acc 上的这些属性，所以桩足够且更稳。
    """
    calls = []

    class Acc(object):
        def __init__(self):
            self.uid = uid
            self.realm = realm
            self.enabled = enabled
            self.access_token = "token-%s" % uid
            self.expires_at = 0          # 0 = 跳过 token 保活分支
            self.last_error = None

        def refresh(self):
            calls.append("refresh")
            return True

        def can_checkin(self):
            calls.append("can_checkin")
            return True

        def checkin(self, trigger=None):
            calls.append("checkin")
            return {"ok": True, "msg": "ok"}

        def can_report_activity(self):
            calls.append("can_report_activity")
            return True

        def report_activity(self):
            calls.append("report_activity")
            return {"ok": True, "msg": "ok"}

        def can_daily_chat(self):
            calls.append("can_daily_chat")
            return True

        def daily_chat(self, trigger=None):
            calls.append("daily_chat")
            return {"ok": True, "msg": "ok", "logs": []}

    acc = Acc()
    return acc, calls


class Pool(object):
    def __init__(self, accounts):
        self.accounts = accounts


class SchedulerEnabledFilterTests(unittest.TestCase):
    def setUp(self):
        # 旁路会联网的猫猫旅行 / 连登管家 / 夜猫子：本套件只关心账号是否被遍历到。
        # 逐个函数替换而不替换整个 wb_tasks 模块 —— Scheduler.__init__ 还要用
        # 它的 set_logger()。
        self._orig_travel = wb_scheduler.do_cat_travel
        self._orig_bonus = wb_scheduler.wb_tasks.run_streak_bonus
        self._orig_night = wb_scheduler.wb_tasks.run_night_growth
        wb_scheduler.do_cat_travel = lambda acc: {"action": "none", "msg": ""}
        wb_scheduler.wb_tasks.run_streak_bonus = lambda acc: {"logs": []}
        wb_scheduler.wb_tasks.run_night_growth = lambda acc: {"logs": []}

    def tearDown(self):
        wb_scheduler.do_cat_travel = self._orig_travel
        wb_scheduler.wb_tasks.run_streak_bonus = self._orig_bonus
        wb_scheduler.wb_tasks.run_night_growth = self._orig_night

    def _run_cycle(self, accounts):
        sched = wb_scheduler.Scheduler(Pool(accounts))
        sched._run_cycle("单测")
        return sched

    def test_disabled_cn_account_is_never_touched(self):
        acc, calls = make_account("cn-off", realm="cn", enabled=False)
        self._run_cycle([acc])
        self.assertEqual(
            calls, [],
            "禁用账号在自动巡检里仍被调用: %r" % (calls,))

    def test_enabled_cn_account_still_runs(self):
        acc, calls = make_account("cn-on", realm="cn", enabled=True)
        self._run_cycle([acc])
        self.assertIn("can_checkin", calls,
                      "过滤写坏了：启用账号也没跑签到")
        self.assertIn("checkin", calls,
                      "过滤写坏了：启用账号也没执行签到")

    def test_disabled_intl_account_skips_daily_chat(self):
        acc, calls = make_account("intl-off", realm="intl", enabled=False)
        self._run_cycle([acc])
        self.assertNotIn(
            "can_daily_chat", calls,
            "禁用国际账号仍被判定为需要每日活跃打卡")

    def test_mixed_pool_only_enabled_account_runs(self):
        off_cn, off_calls = make_account("cn-off", realm="cn", enabled=False)
        off_intl, off_intl_calls = make_account(
            "intl-off", realm="intl", enabled=False)
        on_cn, on_calls = make_account("cn-on", realm="cn", enabled=True)
        self._run_cycle([off_cn, off_intl, on_cn])
        self.assertEqual(off_calls, [], "禁用国内账号被调用: %r" % (off_calls,))
        self.assertEqual(
            off_intl_calls, [],
            "禁用国际账号被调用: %r" % (off_intl_calls,))
        self.assertIn("checkin", on_calls, "启用账号反而没跑")

    def test_empty_pool_still_short_circuits(self):
        sched = self._run_cycle([])
        self.assertIsNotNone(sched.last_run_time,
                             "空池分支被误改")


if __name__ == "__main__":
    unittest.main()