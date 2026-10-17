"""Unit tests for the international realm daily chat check-in.

Two channels are covered: the desktop-identity chat completion (always sent)
and the web conversation added for issue #75/#59, whose request shape is
pinned here so it cannot drift from what the web app actually sends.
"""
import io
import json
import urllib.error, os, sys, unittest, time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from _isolated_dirs import isolated_data_dirs  # noqa: E402  (its own data root)
# Its own directories, removed when this process exits. This suite used to share
# tests/_acc_test and tests/_use_test with _test_sanitize_fingerprint.py and
# _test_tasks_cache.py, which made them unsafe to run at the same time and left
# directories in the repo after a run.
_TMP = isolated_data_dirs("wb-daily-chat-")

import wb_accounts, wb_settings

class DailyChatTests(unittest.TestCase):
    def test_daily_chat_eligibility(self):
        acc = wb_accounts.Account({
            "uid": "test_intl_1",
            "realm": "intl",
            "accessToken": "dummy",
            "lastDailyChat": None
        })
        # Fresh account can chat
        self.assertTrue(acc.can_daily_chat())
        self.assertFalse(acc.can_checkin()) # cn only

        # Already chatted today
        acc.last_daily_chat = time.strftime("%Y-%m-%d 10:00:00")
        self.assertFalse(acc.can_daily_chat())

        # Chatted yesterday
        acc.last_daily_chat = "2020-01-01 10:00:00"
        self.assertTrue(acc.can_daily_chat())

    def test_cn_account_ineligible(self):
        acc = wb_accounts.Account({
            "uid": "test_cn_1",
            "realm": "cn",
            "accessToken": "dummy"
        })
        self.assertFalse(acc.can_daily_chat())
        self.assertTrue(acc.can_checkin())

class WebChannelTests(unittest.TestCase):
    UID = "c90a4e93-edad-42fe-aa3a-133fb27cf6b6"

    def account(self, realm="intl"):
        return wb_accounts.Account({"uid": self.UID, "realm": realm,
                                    "accessToken": "dummy-token", "lastDailyChat": None})

    def stub(self, payload=None, session=None):
        calls = []
        create = payload if payload is not None else {
            "code": 0, "msg": "OK", "data": {"id": "2102411494602919936"}}
        if session is None:
            session = {
                "code": 0, "msg": "OK",
                "data": {"link": "https://box.example/acp", "token": "sandbox-token",
                         "sessionId": "2102411494602919936", "cwd": "/workspace"}}

        class Response(object):
            def __init__(self, payload):
                self.payload = payload

            def read(self, *_args):
                return json.dumps(self.payload).encode("utf-8")

            def __enter__(self):
                return self

            def __exit__(self, *_args):
                return False

        def fake(req, **_kwargs):
            calls.append(req)
            if req.full_url.endswith("/session"):
                return Response(session)
            if req.full_url.endswith("/conversations/"):
                return Response(create)
            return Response({"code": 0, "msg": "OK",
                            "data": {"status": "completed"}})

        return calls, fake

    def stub_turn(self, result=None):
        """Replace the ACP drive with a recorder; returns the recorded calls."""
        seen = []
        real = wb_accounts.wb_webagent.run_turn

        def fake(link, token, session_id, cwd, prompt, user_agent, **kwargs):
            seen.append({"link": link, "token": token, "session_id": session_id,
                         "cwd": cwd, "prompt": prompt,
                         "status": kwargs.get("poll_status")})
            out = {"ok": True, "status": "completed", "events": 5, "chunks": 2,
                   "elapsed_ms": 1234, "error": ""}
            out.update(result or {})
            return out

        wb_accounts.wb_webagent.run_turn = fake
        self.addCleanup(setattr, wb_accounts.wb_webagent, "run_turn", real)
        return seen

    def test_web_conversation_request_shape(self):
        """POST /console/as/conversations/ with the web identity, no X-IDE-*."""
        acc = self.account()
        calls, fake = self.stub()
        turns = self.stub_turn()
        old = wb_accounts.urlopen
        wb_accounts.urlopen = fake
        try:
            res = acc.daily_chat_web()
        finally:
            wb_accounts.urlopen = old
        self.assertTrue(res.get("ok"), res)
        self.assertEqual(res.get("conversation"), "2102411494602919936")
        req = calls[0]
        self.assertEqual(req.method, "POST")
        self.assertEqual(req.full_url, "https://www.workbuddy.ai/console/as/conversations/")
        headers = {k.lower(): v for k, v in req.headers.items()}
        self.assertEqual(headers.get("authorization"), "Bearer dummy-token")
        self.assertEqual(headers.get("x-user-id"), self.UID)
        self.assertEqual(headers.get("origin"), "https://www.workbuddy.ai")
        self.assertFalse([k for k in headers if k.startswith("x-ide")],
                         "the web channel must not carry desktop identity headers")
        body = json.loads(req.data.decode("utf-8"))
        self.assertTrue(body.get("prompt"))
        self.assertEqual(body.get("model"), wb_accounts.DAILY_CHAT_MODEL)
        self.assertEqual(body.get("conversationOrigin"), "workbuddy-app")
        # 建会话之后必须接沙箱、把这一轮跑起来，否则会话永远停在 CREATING。
        self.assertTrue(any(r.full_url.endswith("/session") for r in calls),
                        [r.full_url for r in calls])
        self.assertEqual(len(turns), 1)
        self.assertEqual(turns[0]["link"], "https://box.example/acp")
        self.assertEqual(turns[0]["token"], "sandbox-token")
        self.assertEqual(turns[0]["session_id"], "2102411494602919936")
        self.assertEqual(turns[0]["cwd"], "/workspace")
        self.assertTrue(turns[0]["prompt"])
        self.assertEqual(res.get("status"), "completed")
        self.assertEqual(res.get("chunks"), 2)

    def test_web_conversation_reports_a_business_error(self):
        acc = self.account()
        _calls, fake = self.stub({"code": 12302, "msg": "activity is offline"})
        old = wb_accounts.urlopen
        wb_accounts.urlopen = fake
        try:
            res = acc.daily_chat_web()
        finally:
            wb_accounts.urlopen = old
        self.assertFalse(res.get("ok"))
        self.assertIn("activity is offline", res.get("error", ""))

    def test_web_conversation_is_intl_only(self):
        res = self.account(realm="cn").daily_chat_web()
        self.assertFalse(res.get("ok"))
        self.assertIn("international", res.get("error", ""))

    def test_daily_chat_runs_the_web_step_when_asked(self):
        acc = self.account()
        calls, fake = self.stub()
        self.stub_turn()
        old = wb_accounts.urlopen
        wb_accounts.urlopen = fake
        try:
            res = acc.daily_chat(web=True)
        finally:
            wb_accounts.urlopen = old
        self.assertTrue(res.get("ok"), res)
        self.assertTrue(res.get("web", {}).get("ok"), res)
        self.assertIn("网页通道", res.get("msg", ""))
        self.assertTrue(any(r.full_url.endswith("/console/as/conversations/") for r in calls),
                        [r.full_url for r in calls])

    def test_a_turn_that_never_finishes_is_reported(self):
        acc = self.account()
        calls, fake = self.stub()
        self.stub_turn({"ok": False, "status": "working", "chunks": 0,
                        "error": "会话在 120s 内没有跑完（状态=working）"})
        old = wb_accounts.urlopen
        wb_accounts.urlopen = fake
        try:
            res = acc.daily_chat_web()
        finally:
            wb_accounts.urlopen = old
        self.assertFalse(res.get("ok"), res)
        self.assertEqual(res.get("conversation"), "2102411494602919936")
        self.assertIn("没有跑完", res.get("error", ""))

    def test_a_missing_sandbox_is_reported(self):
        acc = self.account()
        calls, fake = self.stub(session={"code": 0, "msg": "OK", "data": {}})
        self.stub_turn()
        old = wb_accounts.urlopen
        wb_accounts.urlopen = fake
        try:
            res = acc.daily_chat_web()
        finally:
            wb_accounts.urlopen = old
        self.assertFalse(res.get("ok"), res)
        self.assertIn("沙箱", res.get("error", ""))

    def test_daily_chat_skips_the_web_step_when_off(self):
        acc = self.account()
        calls, fake = self.stub()
        old = wb_accounts.urlopen
        wb_accounts.urlopen = fake
        try:
            res = acc.daily_chat(web=False)
        finally:
            wb_accounts.urlopen = old
        self.assertTrue(res.get("ok"), res)
        self.assertNotIn("web", res)
        self.assertFalse(any("/console/as/" in r.full_url for r in calls),
                         [r.full_url for r in calls])

    def test_a_failed_web_step_leaves_the_day_open_for_retry(self):
        """网页腿失败不能写 lastDailyChat：那天还得接着重试，否则 30 积分静默丢掉。"""
        acc = self.account()
        _calls, fake = self.stub()
        self.stub_turn({"ok": False, "status": "working", "chunks": 0,
                        "error": "会话在 120s 内没有跑完（状态=working）"})
        old = wb_accounts.urlopen
        wb_accounts.urlopen = fake
        try:
            res = acc.daily_chat(web=True)
        finally:
            wb_accounts.urlopen = old
        self.assertTrue(res.get("ok"), res)          # 桌面腿成功，返回形状不变
        self.assertFalse(res.get("web", {}).get("ok"), res)
        self.assertIn("网页通道失败", res.get("msg", ""))
        self.assertIsNone(acc.last_daily_chat)       # 但今天不算做过
        self.assertTrue(acc.can_daily_chat())        # 下一次巡检还会重试

    def test_a_finished_web_step_marks_the_day_done(self):
        acc = self.account()
        _calls, fake = self.stub()
        self.stub_turn()
        old = wb_accounts.urlopen
        wb_accounts.urlopen = fake
        try:
            res = acc.daily_chat(web=True)
        finally:
            wb_accounts.urlopen = old
        self.assertTrue(res.get("web", {}).get("ok"), res)
        self.assertTrue(acc.last_daily_chat)
        self.assertFalse(acc.can_daily_chat())

    def test_the_web_step_being_off_still_marks_the_day_done(self):
        acc = self.account()
        _calls, fake = self.stub()
        old = wb_accounts.urlopen
        wb_accounts.urlopen = fake
        try:
            res = acc.daily_chat(web=False)
        finally:
            wb_accounts.urlopen = old
        self.assertTrue(res.get("ok"), res)
        self.assertTrue(acc.last_daily_chat)
        self.assertFalse(acc.can_daily_chat())

    def test_the_web_toggle_defaults_on_and_round_trips(self):
        directory = os.path.join(_TMP.name, "web-toggle")
        self.assertTrue(wb_settings.daily_chat_web(directory))
        self.assertFalse(wb_settings.set_daily_chat_web(directory, False))
        self.assertFalse(wb_settings.daily_chat_web(directory))
        self.assertTrue(wb_settings.set_daily_chat_web(directory, True))
        self.assertTrue(wb_settings.daily_chat_web(directory))
        with open(wb_settings.settings_path(directory), "w", encoding="utf-8") as fh:
            json.dump({"daily_chat_web": "false"}, fh)
        self.assertFalse(wb_settings.daily_chat_web(directory))





class CnCheckinTests(unittest.TestCase):
    def test_already_checked_in_is_treated_as_success(self):
        acc = wb_accounts.Account({"uid": "u-test-cn", "realm": "cn", "accessToken": "token"})
        def fake_http_json(url, data, method, headers, timeout, proxy=""):
            err_body = json.dumps({"code": 10001, "msg": "今天已签到，请明天再来"}).encode("utf-8")
            fp = io.BytesIO(err_body)
            raise urllib.error.HTTPError(url, 400, "Bad Request", {}, fp)

        real_http_json = wb_accounts.http_json
        wb_accounts.http_json = fake_http_json
        try:
            res = acc._checkin_upstream()
            self.assertTrue(res.get("ok"), res)
            self.assertEqual(res.get("code"), 10001)
            self.assertIn("今天已签到", res.get("msg"))
            self.assertTrue(acc.last_checkin)
        finally:
            wb_accounts.http_json = real_http_json


class DailyChatCapTests(unittest.TestCase):
    """每日活跃通道撞 6004 时必须落 model_caps（与转发路径同一套机制）。

    现场（2026-10-17）：调度器连续三次拿 429/6004，activity_history 记了三行
    失败，但账号 JSON 的 modelCaps 一直是空的 —— 因为每日活跃不走 wb_proxy 的
    转发路径，6004 只被当成一条失败日志。这一组把「必须记下来」钉住，同时钉住
    「普通网络失败不能记」——后者若混进来，会让一次 DNS 抖动把模型禁用到第二天。
    """

    UID = "c90a4e93-edad-42fe-aa3a-133fb27cf6b6"
    #: 真实上游原文形状：code 6004 + "reset at <时刻>"
    BODY_6004 = ('{"code":6004,"msg":"usage exceeds frequency limit, but don\'t '
                 'worry, your usage will reset at 2026-10-17 12:00:00"}')

    def account(self):
        return wb_accounts.Account({"uid": self.UID, "realm": "intl",
                                    "accessToken": "dummy-token",
                                    "lastDailyChat": None})

    def http_error(self, code, body):
        """构造一个 urlopen 替身，对任何请求抛指定 HTTPError。"""

        def fake(req, *_args, **_kwargs):
            raise urllib.error.HTTPError(
                getattr(req, "full_url", str(req)), code, "err", {},
                io.BytesIO(body.encode("utf-8")))

        return fake

    def legacy_http_error(self, code, body):
        """老写法（url 作为第一位置参数）同样要能构造出来。

        `Account.http_json` 里既有 `urlopen(req)` 也有 `urlopen(url, data)` 两种
        调用形态，替身必须两种都吃得住，否则测试会因为替身自身的签名而失败，
        而不是因为被测逻辑。
        """

        def fake(url, *_args, **_kwargs):
            raise urllib.error.HTTPError(url, code, "err", {},
                                         io.BytesIO(body.encode("utf-8")))

        return fake

    def _run_web(self, fake):
        acc = self.account()
        old = wb_accounts.urlopen
        wb_accounts.urlopen = fake
        try:
            res = acc._daily_chat_web_upstream()
        finally:
            wb_accounts.urlopen = old
        return acc, res

    def test_web_leg_6004_records_a_model_cap(self):
        acc, res = self._run_web(self.http_error(429, self.BODY_6004))
        self.assertFalse(res.get("ok"), res)
        self.assertIn("6004", res.get("error", ""))       # 返回值形状不变
        self.assertEqual(acc.capped_model_names(), [wb_accounts.DAILY_CHAT_MODEL])
        self.assertGreater(acc.cap_until(wb_accounts.DAILY_CHAT_MODEL), time.time())

    def test_reset_survives_the_returned_error_truncation(self):
        """返回给调用方的 error 截断到 120 字符，但解析必须用完整原文。

        上游把 reset 时刻放在第 120 字符之后是常态（前面还有一段英文长句），
        若图省事拿返回值里那份 detail[:120] 去解析，就会取不到重置时刻、静默
        不记 —— 断言 cap 落地即钉住了这一点。
        """
        long_body = ('{"code":6004,"msg":"' + "x" * 200 +
                     ' usage exceeds frequency limit, but don\'t worry, your '
                     'usage will reset at 2026-10-17 12:00:00"}')
        acc, res = self._run_web(self.http_error(429, long_body))
        self.assertFalse(res.get("ok"), res)
        self.assertLessEqual(len(res.get("error", "")), 130)   # 返回值仍是截断的
        self.assertEqual(acc.capped_model_names(), [wb_accounts.DAILY_CHAT_MODEL])

    def test_plain_network_failure_records_nothing(self):
        """普通 DNS/连接失败绝不能被记成 cap，否则一次抖动就白禁一天。"""

        def fake(url, *_args, **_kwargs):
            raise OSError("[Errno -3] Try again")

        acc, res = self._run_web(fake)
        self.assertFalse(res.get("ok"), res)
        self.assertEqual(acc.capped_model_names(), [])
        self.assertEqual(acc.model_caps, {})

    def test_non_6004_rate_limit_records_nothing(self):
        """只有 6004 才算「窗口用满」；其它 429（如 intl 14003）是软限，不落 cap。"""
        acc, res = self._run_web(self.http_error(
            429, '{"code":14003,"msg":"usage exceeds frequency limit"}'))
        self.assertFalse(res.get("ok"), res)
        self.assertEqual(acc.model_caps, {})

    def test_desktop_leg_6004_also_records_a_model_cap(self):
        """桌面腿（_daily_chat_upstream）撞 6004 时同样要记。"""
        acc = self.account()
        old = wb_accounts.urlopen
        wb_accounts.urlopen = self.legacy_http_error(429, self.BODY_6004)
        try:
            res = acc._daily_chat_upstream(web=False)
        finally:
            wb_accounts.urlopen = old
        self.assertFalse(res.get("ok"), res)
        self.assertEqual(acc.capped_model_names(), [wb_accounts.DAILY_CHAT_MODEL])


if __name__ == "__main__":
    unittest.main()
