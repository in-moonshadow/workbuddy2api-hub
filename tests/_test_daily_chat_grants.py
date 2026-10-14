"""每日活跃打卡的完成判定（D2）：本地快照优先，快照缺失回落原逻辑。

`can_daily_chat()`（wb_accounts.py:1282）原本只看本地 `last_daily_chat` 字符串的
日期前缀，从不向服务端核实「今天到底领到没有」。跨时区或进程崩溃后会误判：
打卡跑了但服务端没发积分，它照样记下时间戳，下次直接跳过。

账号对象本来就带一份积分快照 `self.credits`（由 `fetch_credits()` 刷新、
`to_dict()` 落盘），其中每个积分包带 `name` / `grant_reason` / `size` /
`create_time` —— 足够判定「今日是否已发活跃奖励」，且**读它不发上游请求**。
本套件钉住「快照优先 + 回落」这条语义：

  1. 快照里有今日活跃奖励包 → 判定已完成（即便 last_daily_chat 是旧的/空的）；
  2. 快照里没有今日奖励包 → 回落原 last_daily_chat 逻辑（不改变既有行为）；
  3. 快照缺失 / 结构异常 → 回落，绝不因读快照而崩；
  4. 边界：昨天有包今天没有 → 不能被当成「今日已完成」；
  5. grant_reason 非空、或 size 不是 30/50 的包不算活跃奖励；
  6. realm != intl 仍然直接 False（国际端专属约束不变）。

⚠️ 刻意保留的代价：快照陈旧时（面板没手动刷新积分）可能多跑一次打卡，
换少量积分消耗。方向是「宁可多做，不要漏做」。

Run with the current interpreter (python tests/run_all.py daily_chat_grants).
"""
import os
import sys
import time
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import wb_accounts


def today_stamp(offset_days=0):
    """相对今天偏移若干天的 "YYYY-MM-DD HH:MM:SS"，用于造 create_time。"""
    return time.strftime(
        "%Y-%m-%d %H:%M:%S", time.localtime(time.time() - offset_days * 86400))


def make_pkg(name, size, grant_reason="", created=None, uid="u1"):
    return {
        "uid": uid,
        "name": name,
        "size": size,
        "grant_reason": grant_reason,
        "create_time": created if created is not None else today_stamp(),
    }


def make_account(realm="intl", last_daily_chat=None, packages=None,
                 credits=True):
    data = {
        "uid": "intl-1",
        "realm": realm,
        "accessToken": "dummy-token",
        "lastDailyChat": last_daily_chat,
    }
    if credits:
        data["credits"] = {
            "packages": list(packages or []),
            "updated_at": time.time(),
        }
    return wb_accounts.Account(data)


class DailyChatGrantSnapshotTests(unittest.TestCase):
    def test_snapshot_grant_today_marks_done(self):
        """快照里今日已有活跃奖励包 -> 判定已完成。"""
        acc = make_account(packages=[
            make_pkg("Daily Activity Bonus", 30),
        ])
        self.assertFalse(
            acc.can_daily_chat(),
            "今日奖励包已在快照里，却仍判定为需要打卡")

    def test_snapshot_grant_today_beats_stale_local_marker(self):
        """last_daily_chat 是昨天但服务端今日已发 -> 仍判定完成。

        这正是 D2 要修的反向误判：本地以为昨天做的就跳过，但服务端今天
        已经发过，说明今天确实完成过。
        """
        acc = make_account(last_daily_chat=today_stamp(offset_days=1),
                           packages=[make_pkg("Daily Activity Bonus", 50)])
        self.assertFalse(acc.can_daily_chat())

    def test_no_grant_today_falls_back_to_local_logic(self):
        """快照里没有今日奖励包 -> 回落原逻辑（今天没做过则仍可做）。"""
        acc = make_account(last_daily_chat=None, packages=[
            make_pkg("Some Other Package", 100),
        ])
        self.assertTrue(
            acc.can_daily_chat(),
            "无今日奖励包且本地未记录时，应回落为可打卡")

    def test_yesterdays_grant_does_not_block_today(self):
        """边界：昨天有包、今天没有 -> 不能被当成今日已完成。"""
        acc = make_account(last_daily_chat=None, packages=[
            make_pkg("Daily Activity Bonus", 30,
                     created=today_stamp(offset_days=1)),
        ])
        self.assertTrue(
            acc.can_daily_chat(),
            "昨天的奖励包不该挡住今天的打卡")

    def test_non_empty_grant_reason_is_not_activity_bonus(self):
        """grant_reason 非空的包不算活跃奖励（口径要求为空）。"""
        acc = make_account(packages=[
            make_pkg("Daily Activity Bonus", 30,
                     grant_reason="manual_adjustment"),
        ])
        self.assertTrue(acc.can_daily_chat())

    def test_unexpected_size_is_not_activity_bonus(self):
        """size 不是 30/50 的 bonus 包不算活跃奖励。"""
        acc = make_account(packages=[
            make_pkg("Daily Activity Bonus", 99),
        ])
        self.assertTrue(acc.can_daily_chat())

    def test_missing_credits_snapshot_falls_back(self):
        """快照整体缺失 -> 回落本地逻辑，不崩。"""
        acc = make_account(credits=False)
        self.assertTrue(acc.can_daily_chat())

    def test_malformed_snapshot_does_not_raise(self):
        """快照结构异常（packages 非列表 / 元素非 dict）-> 不抛异常。"""
        for bad in ([], "not-a-list", None, [None, 42, "x"]):
            acc = make_account()
            acc.credits = {"packages": bad}
            self.assertTrue(
                acc.can_daily_chat(),
                "畸形快照 packages=%r 应回落而非抛错" % (bad,))

    def test_cn_account_still_ineligible(self):
        """realm != intl 仍直接 False，国际端专属约束不变。"""
        acc = make_account(realm="cn", packages=[
            make_pkg("Daily Activity Bonus", 30),
        ])
        self.assertFalse(acc.can_daily_chat())

    def test_name_without_bonus_keyword_is_not_matched(self):
        """包名不含 bonus 关键字 -> 不算活跃奖励。"""
        acc = make_account(packages=[
            make_pkg("Daily Reward", 30),
        ])
        self.assertTrue(acc.can_daily_chat())


if __name__ == "__main__":
    unittest.main()