from datetime import datetime, timezone
import unittest
from unittest.mock import patch
import auto_release as a

class PolicyTests(unittest.TestCase):
    def test_first_release_boundary(self):
        now=a.timestamp('2026-09-15T00:00:00Z')
        self.assertIsNone(a.cooldown_reason([],[],now,'2026-09-01T00:00:00Z'))
        self.assertIsNotNone(a.cooldown_reason([],[],now,'2026-09-01T00:00:01Z'))
    def test_previous_release_controls_timer_not_pr_activity(self):
        releases=[{'draft':False,'prerelease':False,'tag_name':'fastrace-diesel-0.1.0','published_at':'2026-09-01T00:00:00Z'}]
        self.assertIsNone(a.cooldown_reason(releases,[],a.timestamp('2026-09-15T00:00:00Z'),'2026-09-14T00:00:00Z'))
        self.assertIsNotNone(a.cooldown_reason(releases,[],a.timestamp('2026-09-14T23:59:59Z'),'2026-01-01T00:00:00Z'))
    def test_unpublished_merge_blocks_first_and_later_release(self):
        closed=[{'merged_at':'2026-09-01T00:00:00Z'}]
        self.assertIn('awaiting',a.cooldown_reason([],closed,datetime.now(timezone.utc),'2026-01-01T00:00:00Z'))
    def test_ci_requires_all_jobs_and_latest_success(self):
        runs=[{'id':1,'status':'completed','conclusion':'success'}]
        jobs=[{'name':n,'status':'completed','conclusion':'success'} for n in a.REQUIRED_JOBS]
        self.assertTrue(a.ci_passed(runs,jobs))
        self.assertFalse(a.ci_passed(runs,jobs[:1]))
        self.assertFalse(a.ci_passed(runs+[{'id':2,'status':'queued','conclusion':None}],jobs))
    def test_fork_is_not_release_pr(self):
        pr={'head':{'ref':'release-plz-next','repo':{'full_name':'other/repo'}},'base':{'ref':'main'}}
        self.assertFalse(a.release_pr(pr,'codyps/fastrace-diesel','main'))
    def test_pagination_limit_fails_closed(self):
        with patch.object(a,'api',return_value=[{}]*100):
            with self.assertRaises(RuntimeError): a.pages('example')

if __name__=='__main__': unittest.main()
