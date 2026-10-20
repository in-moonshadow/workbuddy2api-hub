"""每日活跃打卡的**同日完成判定**：本地证据为准，上游奖励包按 D+1 口径兜底。

`can_daily_chat()`（wb_accounts.py）回答的是「今天还要不要跑一次每日活跃对话」。
判据必须落在**今天**的证据上，而上游奖励包跨日发放：

    对话发生在 D 天 -> 奖励包的 create_time 落在 D+1 天。

2026-10-20 逐包核对（脚本 ~/scratch/wb-dailychat-diag/verify_grant_day_rule.py）：
20 个 30/50 奖励包里，19 个的**前一日**有成功对话，且 20/20 的发放时刻都早于
当日首次成功对话（同日口径 0/20 成立）。唯一例外 42203090215 的 10-18 包对应它
10-17 那次「客户端报 11128、上游仍然计数」的对话，反过来印证 D+1。

⚠️ 被更正的口径（留档，勿再移植）：本套件早先钉的是「积分快照里有**今天**的
30/50 奖励包 ⇒ 今日已完成」（即 D2 那版）。按 D+1 规则，「今天的包」说的是
**昨天**的对话已结算，用它挡今天的对话会让每个账号隔天才拿一包：今天被昨天的
包挡住 -> 今天没有对话 -> 明天没有包 -> 循环卡住。这是本套件现在钉的回归点。

本套件钉住的语义：

  1. 今天有落盘标记 `lastDailyChat` -> 今日已完成；
  2. 今天的积分包**不能**当作今日已完成（D+1，那是昨天的结算）；
  3. **明天**日期的积分包 -> 今日已被上游记账（兜底口径成立）；
  4. 活动历史里今天有「网页通道 completed」的成功记录 -> 今日已完成（落盘失败
     时的兜底）；历史里今天那条是「网页通道失败」-> 今日**没**完成（那种 ok=True
     并未计入官方活跃，误判会让当天积分静默丢掉）；
  5. 快照缺失 / 结构异常 / 读历史失败 -> 一律回落，绝不因读它们而崩；
  6. 边界：grant_reason 非空、size 不是 30/50、包名不含 bonus 的包都不算活跃奖励；
  7. realm != intl 仍然直接 False（国际端专属约束不变）。

⚠️ 刻意保留的代价：判不出来一律当作「没做」——宁可多做一次（多耗一点账号
积分），不要漏做（丢掉当天的 30/50）。

Run with the current interpreter (python tests/run_all.py daily_chat_grants).
"""
import os
import shutil
import sys
import tempfile
import time
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import wb_accounts
import wb_activity


def today_stamp(offset_days=0):
    """相对今天偏移若干天的 "YYYY-MM-DD HH:MM:S"，用于造 create_time。

    offset_days=1 是昨天，-1 是明天（明天对应「今天的对话已被上游记账」）。
    """
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
                 credits=True, uid="intl-1"):
    data = {
        "uid": uid,
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


class DailyChatCompletionTests(unittest.TestCase):
    def setUp(self):
        # 活动历史必须落到临时目录，绝不能碰生产 usage/ 下的真文件。
        self._tmp = tempfile.mkdtemp(prefix="wb-daily-chat-grants-")
        wb_activity.set_data_dir(self._tmp)

    def tearDown(self):
        wb_activity.set_data_dir(None)
        shutil.rmtree(self._tmp, ignore_errors=True)

    def write_history(self, uid="intl-1", message=None, ok=True,
                      task="daily_chat"):
        wb_activity.record(uid=uid, nickname="nick", realm="intl", task=task,
                           trigger="scheduler", ok=ok,
                           message=message if message is not None else "ok")

    # --- 本地落盘标记 ---------------------------------------------------

    def test_local_marker_today_marks_done(self):
        """今天有落盘标记 -> 今日已完成。"""
        acc = make_account(last_daily_chat=today_stamp())
        self.assertFalse(
            acc.can_daily_chat(),
            "今天已跑过每日活跃对话，却仍判定为需要跑")

    def test_local_marker_yesterday_does_not_block_today(self):
        """昨天的标记不能挡住今天。"""
        acc = make_account(last_daily_chat=today_stamp(offset_days=1))
        self.assertTrue(acc.can_daily_chat())

    # --- 上游奖励包（D+1 口径） ------------------------------------------

    def test_todays_bonus_package_does_not_block_today(self):
        """回归点：今天的积分包 = 昨天的对话已结算，不能当成今日已完成。

        旧口径在这里判 False，于是每天清晨包一落地就把当天的对话挡掉，账号
        变成隔天才拿一包。
        """
        acc = make_account(last_daily_chat=today_stamp(offset_days=1),
                           packages=[make_pkg("Daily Activity Bonus", 30)])
        self.assertTrue(
            acc.can_daily_chat(),
            "今天的奖励包是昨天的结算，不该挡住今天的对话")

    def test_todays_package_without_local_record_does_not_block(self):
        """没有落盘标记时，今天的包同样不能挡（包只说明昨天发过奖）。"""
        acc = make_account(packages=[make_pkg("Daily Activity Bonus", 50)])
        self.assertTrue(acc.can_daily_chat())

    def test_tomorrows_bonus_package_marks_done(self):
        """明天日期的包 = 今天的对话已被上游记账 -> 今日已完成（兜底口径）。"""
        acc = make_account(packages=[
            make_pkg("Daily Activity Bonus", 30,
                     created=today_stamp(offset_days=-1)),
        ])
        self.assertFalse(
            acc.can_daily_chat(),
            "明天的包说明今天已被上游记账，却仍判定为需要跑")

    def test_non_empty_grant_reason_is_not_activity_bonus(self):
        """grant_reason 非空的包不算活跃奖励（口径要求为空）。"""
        acc = make_account(packages=[
            make_pkg("Daily Activity Bonus", 30,
                     grant_reason="manual_adjustment",
                     created=today_stamp(offset_days=-1)),
        ])
        self.assertTrue(acc.can_daily_chat())

    def test_unexpected_size_is_not_activity_bonus(self):
        """size 不是 30/50 的 bonus 包不算活跃奖励。"""
        acc = make_account(packages=[
            make_pkg("Daily Activity Bonus", 99,
                     created=today_stamp(offset_days=-1)),
        ])
        self.assertTrue(acc.can_daily_chat())

    def test_name_without_bonus_keyword_is_not_matched(self):
        """包名不含 bonus 关键字 -> 不算活跃奖励。"""
        acc = make_account(packages=[
            make_pkg("Daily Reward", 30, created=today_stamp(offset_days=-1)),
        ])
        self.assertTrue(acc.can_daily_chat())

    # --- 活动历史兜底 ----------------------------------------------------

    def test_history_web_ok_today_marks_done(self):
        """历史里今天有「网页通道 completed」的成功记录 -> 今日已完成。

        这是落盘标记丢失（账号文件写盘失败）时的兜底：没有它，当天后续每次
        巡检都会重跑一次完整对话。
        """
        self.write_history(
            message="每日活跃对话成功完成（网页通道 completed：6 段输出，15953 ms）")
        acc = make_account()
        self.assertFalse(
            acc.can_daily_chat(),
            "历史里今天已跑完，却仍判定为需要跑")

    def test_history_web_failed_today_does_not_mark_done(self):
        """历史里今天那条是「网页腿失败」-> 今日**没**完成，可以再跑。

        那种尝试同样是 ok=True，但没计入官方每日活跃；当成已完成会让当天的
        30/50 积分静默丢掉。
        """
        self.write_history(
            message="每日活跃对话成功完成（网页通道失败：会话在 120s 内没有跑完（状态=working））")
        acc = make_account()
        self.assertTrue(
            acc.can_daily_chat(),
            "网页腿失败的尝试并未计入活跃，不该被当成今日已完成")

    def test_history_from_another_account_does_not_mark_done(self):
        """别人的历史不能算到本账号头上。"""
        self.write_history(
            uid="other-uid",
            message="每日活跃对话成功完成（网页通道 completed：6 段输出）")
        acc = make_account(uid="intl-1")
        self.assertTrue(acc.can_daily_chat())

    def test_history_without_uid_does_not_match_any_row(self):
        """账号没有 uid 时不去猜，历史里有别人的记录也不算数。"""
        self.write_history(
            uid="other-uid",
            message="每日活跃对话成功完成（网页通道 completed：6 段输出）")
        acc = make_account(uid="")
        self.assertTrue(acc.can_daily_chat())

    def test_history_read_failure_falls_back_without_raising(self):
        """读历史失败 -> 回落本地标记，绝不抛异常。"""
        old_query = wb_activity.query

        def boom(*_args, **_kwargs):
            raise OSError("history unavailable")

        wb_activity.query = boom
        try:
            acc = make_account(last_daily_chat=today_stamp())
            self.assertFalse(acc.can_daily_chat())
            acc2 = make_account()
            self.assertTrue(acc2.can_daily_chat())
        finally:
            wb_activity.query = old_query

    # --- 快照缺失/异常 ----------------------------------------------------

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


if __name__ == "__main__":
    unittest.main()
