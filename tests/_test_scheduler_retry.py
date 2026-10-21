"""调度器「网络类失败延后补跑」（D48）。

背景（2026-10-21 定位）：宿主 Clash Verge 每天 08:2x 自动更新订阅并重载
Mihomo core，容器出站被宿主 TUN 承载 ⇒ 在途 TCP 被打断 ⇒ 签到/每日活跃
批量报 `SSL: UNEXPECTED_EOF_WHILE_READING`（偶发 `Errno -3` DNS 临时失败）。
中断持续 1-3 分钟，而调度器的整点重试最近的一次在 21:00（12 小时后），
于是早上失败只能靠用户手动补跑。

本套件钉住补跑机制本身：

  1. 网络类错误串被识别为「值得延后补跑」；业务类错误（11128 / 6004 /
     「今日已做」）不识别——重试它们只会白白发请求、吃上游限流；
  2. 巡检里因网络故障失败的签到 / 每日活跃会进补跑队列；
  3. 到期后补跑真的再调一次业务方法；成功即出队；
  4. 补跑仍遇网络故障 ⇒ 按上限重新排队（最多 3 次），业务类失败不再排；
  5. 补跑前复查门禁：别人已经做完了就不再打一次；
  6. 账号已停用 / 已不在池里 ⇒ 取消补跑。

全程零网络、零凭证：账号是桩，`do_cat_travel` / `run_streak_bonus` /
`run_night_growth` 全被旁路。

Run with the current interpreter (python tests/run_all.py scheduler_retry).
"""
import os
import sys
import time
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import wb_scheduler

SSL_EOF = ("<urlopen error [SSL: UNEXPECTED_EOF_WHILE_READING] "
           "EOF occurred in violation of protocol (_ssl.c:1016)>")
DNS_AGAIN = "<urlopen error [Errno -3] Try again>"


def make_account(uid, realm="cn", enabled=True, results=None):
    """账号桩：results 是 [(ok, msg_or_error), ...] 依次返回，缺省即成功。

    业务方法被替换成计数器，完全不碰网络。
    """
    calls = []
    pending = list(results or [])

    class Acc(object):
        def __init__(self):
            self.uid = uid
            self.realm = realm
            self.enabled = enabled
            self.access_token = "token-%s" % uid
            self.expires_at = 0
            self.last_error = None
            self.done = False      # can_* 的门闸

        def refresh(self):
            calls.append("refresh")
            return True

        def can_checkin(self):
            calls.append("can_checkin")
            return not self.done

        def checkin(self, trigger=None):
            calls.append("checkin")
            if pending:
                ok, detail = pending.pop(0)
            else:
                ok, detail = True, "ok"
            if ok:
                return {"ok": True, "msg": detail}
            return {"ok": False, "error": detail}

        def can_report_activity(self):
            calls.append("can_report_activity")
            return False

        def report_activity(self):
            calls.append("report_activity")
            return {"ok": True, "msg": "ok"}

        def can_daily_chat(self):
            calls.append("can_daily_chat")
            return not self.done

        def daily_chat(self, trigger=None):
            calls.append("daily_chat")
            if pending:
                ok, detail = pending.pop(0)
            else:
                ok, detail = True, "ok"
            if ok:
                return {"ok": True, "msg": detail, "logs": []}
            return {"ok": False, "error": detail}

    acc = Acc()
    return acc, calls


class Pool(object):
    def __init__(self, accounts):
        self.accounts = accounts


class SchedulerRetryTests(unittest.TestCase):
    def setUp(self):
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

    def _sched(self, accounts):
        return wb_scheduler.Scheduler(Pool(accounts))

    # --- 1：分类 ---------------------------------------------------------

    def test_network_error_strings_are_recognised(self):
        for text in (SSL_EOF, DNS_AGAIN, "请求超时",
                     "<urlopen error [Errno 111] Connection refused>",
                     "error sending request for url (https://x/)"):
            self.assertTrue(wb_scheduler._is_network_failure(text),
                            "网络类错误没被认出来: %r" % (text,))

    def test_business_errors_are_not_network_failures(self):
        for text in ('HTTP 400: {"code":11128,"msg":"request illegal"}',
                     'HTTP 429: {"code":6004,"msg":"usage exceeds frequency limit"}',
                     "今天已签到", "今日已做或不可做", ""):
            self.assertFalse(wb_scheduler._is_network_failure(text),
                             "业务类错误被当成网络故障了: %r" % (text,))

    # --- 2：入队 ---------------------------------------------------------

    def test_cycle_queues_network_failure_for_retry(self):
        acc, _ = make_account("cn-net", realm="cn",
                              results=[(False, SSL_EOF)])
        sched = self._sched([acc])
        sched._run_cycle("单测")
        pending = sched._pending_retries
        self.assertEqual(len(pending), 1, "网络故障没有进补跑队列: %r" % (pending,))
        self.assertEqual(pending[0]["uid"], "cn-net")
        self.assertEqual(pending[0]["task"], "checkin")

    def test_business_failure_is_not_queued(self):
        acc, _ = make_account("cn-biz", realm="cn",
                              results=[(False, 'HTTP 400: {"code":11128}')])
        sched = self._sched([acc])
        sched._run_cycle("单测")
        self.assertEqual(sched._pending_retries, [],
                         "业务类失败不该进补跑队列: %r" % (sched._pending_retries,))

    def test_intl_daily_chat_network_failure_is_queued(self):
        acc, _ = make_account("intl-net", realm="intl",
                              results=[(False, SSL_EOF)])
        sched = self._sched([acc])
        sched._run_cycle("单测")
        pending = sched._pending_retries
        self.assertEqual(len(pending), 1, "国际版每日活跃的网络故障没进队列")
        self.assertEqual(pending[0]["task"], "daily_chat")

    # --- 3：补跑 ---------------------------------------------------------

    def _make_due(self, sched):
        for item in sched._pending_retries:
            item["due"] = time.time() - 1

    def test_due_retry_runs_again_and_succeeds(self):
        acc, calls = make_account("cn-net", realm="cn",
                                  results=[(False, SSL_EOF), (True, "签到成功")])
        sched = self._sched([acc])
        sched._run_cycle("单测")
        self._make_due(sched)
        sched._run_due_retries()
        self.assertEqual(calls.count("checkin"), 2, "补跑没有再调一次签到: %r" % (calls,))
        self.assertEqual(sched._pending_retries, [], "成功后没出队")
        self.assertTrue(any("补跑成功" in line for line in sched.logs),
                        "补跑成功没进日志: %r" % (sched.logs,))

    def test_retry_failure_requeues_until_cap(self):
        acc, calls = make_account("cn-net", realm="cn",
                                  results=[(False, SSL_EOF)] * 6)
        sched = self._sched([acc])
        sched._run_cycle("单测")
        for _ in range(6):
            self._make_due(sched)
            sched._run_due_retries()
        # 初次失败 + 最多 RETRY_MAX_TRIES 次补跑，之后不再排
        self.assertEqual(
            calls.count("checkin"),
            1 + wb_scheduler.Scheduler.RETRY_MAX_TRIES,
            "补跑次数没封顶: %r" % (calls,))
        self.assertEqual(sched._pending_retries, [],
                         "超过上限仍在排队: %r" % (sched._pending_retries,))

    def test_business_failure_during_retry_is_not_requeued(self):
        acc, _ = make_account("cn-net", realm="cn",
                              results=[(False, SSL_EOF),
                                       (False, 'HTTP 429: {"code":6004}')])
        sched = self._sched([acc])
        sched._run_cycle("单测")
        self._make_due(sched)
        sched._run_due_retries()
        self.assertEqual(sched._pending_retries, [],
                         "业务类失败不该再排补跑: %r" % (sched._pending_retries,))

    # --- 4：补跑前的门禁 --------------------------------------------------

    def test_retry_skipped_when_task_already_done(self):
        acc, calls = make_account("cn-net", realm="cn",
                                  results=[(False, SSL_EOF)])
        sched = self._sched([acc])
        sched._run_cycle("单测")
        acc.done = True          # 期间别人已经做完了
        self._make_due(sched)
        sched._run_due_retries()
        self.assertEqual(calls.count("checkin"), 1,
                         "已完成的任务被重复打了: %r" % (calls,))
        self.assertEqual(sched._pending_retries, [], "取消后没出队")

    def test_retry_skipped_when_account_left_pool(self):
        acc, _ = make_account("cn-net", realm="cn",
                              results=[(False, SSL_EOF)])
        sched = self._sched([acc])
        sched._run_cycle("单测")
        sched.pool.accounts = []          # 账号被移除
        self._make_due(sched)
        sched._run_due_retries()
        self.assertEqual(sched._pending_retries, [], "账号不在池里却还留着待办")

    def test_retry_not_due_yet_is_left_alone(self):
        acc, calls = make_account("cn-net", realm="cn",
                                  results=[(False, SSL_EOF)])
        sched = self._sched([acc])
        sched._run_cycle("单测")
        sched._run_due_retries()           # 还没到期
        self.assertEqual(calls.count("checkin"), 1,
                         "没到期的补跑被提前执行了: %r" % (calls,))
        self.assertEqual(len(sched._pending_retries), 1, "没到期的待办丢了")


if __name__ == "__main__":
    unittest.main()
