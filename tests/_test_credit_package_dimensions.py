"""积分包取数口径：累计口径与周期口径必须整组同维度择一。

上游 `get-user-resource` 每个积分包给两套额度字段，都是字符串数字：

  - 累计口径：CapacitySizePrecise / CapacityRemainPrecise / CapacityUsedPrecise
  - 周期口径：CycleCapacitySizePrecise / CycleCapacityRemainPrecise / CycleCapacityUsedPrecise

两套口径会给出相反的「剩余」读数，取决于包的形态：

  - 一次性赠额未动（累计剩 100、周期剩 0）：只认周期口径会算成 0（本次修掉的 bug）；
  - 月度包周期扣满（累计剩 500、周期剩 0）：只认累计口径会算成还有 500。

所以按「剩余较大的那一口径」取，且 size/used/remain **整组同维度**，不能混搭
（混搭会让 `used = size - remain` 的补正算出荒谬值）。判定口径时必须用能区分
「值就是 0」与「字段缺失」的取值函数，否则周期那侧返回 0.0 会让判断恒真、
永不回落累计。

Run with the current interpreter (python tests/run_all.py credit).
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import wb_accounts  # noqa: E402


def make_account(uid="u1"):
    return wb_accounts.Account({"uid": uid, "accessToken": "token-%s" % uid,
                                "realm": "intl"})


def raw_package(cumulative=None, cycle=None, name="P"):
    """按上游形状造一个包；两参都是 (size, remain, used) 三元组或 None。

    数值按上游惯例写成字符串。包名里带 Free Plan 便于对账，取值逻辑与包名无关。
    """
    acc = {"PackageName": name, "PackageCode": "pkg",
           "CycleEndTime": "2026-10-31 23:59:59"}
    for prefix, triple in (("", cumulative), ("Cycle", cycle)):
        if triple is None:
            continue
        size, remain, used = triple
        acc[prefix + "CapacitySizePrecise"] = str(size)
        acc[prefix + "CapacityRemainPrecise"] = str(remain)
        acc[prefix + "CapacityUsedPrecise"] = str(used)
    return acc


class PackageDimensionTests(unittest.TestCase):
    def parse(self, **kwargs):
        return make_account()._parse_package_account(raw_package(**kwargs))

    # ---- 本次 bug：一次性赠额未动，累计有 100、周期为 0 -----------------

    def test_one_off_grant_untouched_reads_cumulative(self):
        p = self.parse(cumulative=(100, 100, 0), cycle=(100, 0, 100))
        self.assertEqual(p["remain"], 100)
        self.assertEqual(p["size"], 100)
        self.assertEqual(p["used"], 0)

    def test_one_off_grant_spent_reads_zero(self):
        p = self.parse(cumulative=(100, 0, 100), cycle=(100, 0, 100))
        self.assertEqual(p["remain"], 0)
        self.assertEqual(p["size"], 100)
        self.assertEqual(p["used"], 100)

    # ---- 月度包周期扣满：累计看似满额、周期已归零，取累计 -----------------

    def test_monthly_package_cycled_out_reads_cumulative(self):
        p = self.parse(cumulative=(500, 500, 0), cycle=(500, 0, 500))
        self.assertEqual(p["remain"], 500)
        self.assertEqual(p["size"], 500)
        self.assertEqual(p["used"], 0)

    # ---- 周期口径更大时取周期，且 size/used 同维度 -----------------------

    def test_larger_cycle_dimension_wins_whole_group(self):
        p = self.parse(cumulative=(100, 10, 90), cycle=(600, 600, 0))
        self.assertEqual(p["remain"], 600)
        self.assertEqual(p["size"], 600)
        self.assertEqual(p["used"], 0)

    # ---- 周期字段缺失时回落累计 -----------------------------------------

    def test_missing_cycle_fields_fall_back_to_cumulative(self):
        p = self.parse(cumulative=(100, 100, 0))
        self.assertEqual(p["remain"], 100)
        self.assertEqual(p["size"], 100)
        self.assertEqual(p["used"], 0)

    # ---- 只有周期字段时按周期读（老形状不能回归） ------------------------

    def test_cycle_only_package_still_reads_cycle(self):
        p = self.parse(cycle=(250, 0, 250))
        self.assertEqual(p["remain"], 0)
        self.assertEqual(p["size"], 250)
        self.assertEqual(p["used"], 250)

    # ---- 两套字段都没有时不炸，给出 0 -----------------------------------

    def test_no_capacity_fields_yields_zero(self):
        p = self.parse()
        self.assertEqual(p["remain"], 0)
        self.assertEqual(p["size"], 0)


if __name__ == "__main__":
    unittest.main()
