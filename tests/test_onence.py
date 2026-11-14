"""The driver against recorded 1NCE responses. Every value is invented: no real
ICCID, address, account or credential appears here."""

import datetime
from dataclasses import dataclass

import pytest

from pulzr_connectivity import Capabilities, UpstreamError
from pulzr_connectivity.testkit import Replay, check_capabilities
from pulzr_connectivity_1nce import OneNce
from pulzr_connectivity_1nce.driver import API_BASE

ICCID = '8988000000000000001'


@dataclass(frozen=True)
class Creds:
    namespace: str = ''
    client_id: str = 'test_user'
    client_secret: str = 'test_secret'


SIM = {'iccid': ICCID, 'status': {'id': 1, 'description': 'Enabled'}, 'ip_address': '10.0.0.7',
       'imei': 'null', 'imei_lock': False, 'label': 'rooftop', 'current_quota': 400.0}
RECORDINGS = {
    ('POST', '/oauth/token'): {'access_token': 'tok', 'expires_in': 3600},
    ('GET', f'/v1/sims/{ICCID}'): SIM,
    ('PUT', f'/v1/sims/{ICCID}'): None,
    ('POST', f'/v1/sims/{ICCID}/reset'): {},
    ('POST', f'/v1/sims/{ICCID}/topup?payment_method=monthlyinvoice'): {},
    ('GET', f'/v1/sims/{ICCID}/quota/data'): {'volume': 400.0, 'total_volume': 500.0},
    ('GET', f'/v1/sims/{ICCID}/status'): {'status': 'ONLINE', 'pdp_context': {
        'rat_type': {'description': 'LTE'}, 'ue_ip_address': '10.0.0.7'}},
    ('GET', f'/v1/sims/{ICCID}/connectivity_info'): {'subscriber_info': {'success': False, 'error': 'x'}},
    ('GET', f'/v1/sims/{ICCID}/usage'): {'stats': [
        {'date': '2026-01-02', 'data': {'volume': 2, 'volume_tx': 1, 'volume_rx': 1}},
        {'date': 'TOTAL', 'data': {'volume': 2}},
        {'date': '2026-01-01', 'data': {'volume': 1}}]},
}


@pytest.fixture
def replay(monkeypatch):
    recordings = dict(RECORDINGS)
    recordings[('GET', '/v1/sims')] = [{**SIM, 'iccid': f'{ICCID[:-1]}{i}'} for i in range(3)]
    monkeypatch.setenv('CELLULAR_TOPUPS_ENABLED', 'true')
    monkeypatch.setenv('ONENCE_TOPUP_PAYMENT_METHOD', 'monthlyinvoice')
    with Replay(recordings, base_url=API_BASE) as r:
        yield r


def test_it_declares_only_what_it_implements(replay):
    check_capabilities(OneNce(), Creds(), ICCID)


def test_check_proves_the_credentials_and_returns_capabilities(replay):
    caps = OneNce().check(Creds(), {})
    assert isinstance(caps, Capabilities) and 'sim.reset' in caps
    assert ('POST', '/oauth/token') in replay.calls


def test_a_sim_is_normalized(replay):
    sim = OneNce().sim(Creds(), {}, ICCID)
    assert sim['ipAddress'] == '10.0.0.7'
    assert sim['status'] == 'Enabled'
    assert sim['imei'] is None, '"null" is how 1NCE says unset'


def test_the_account_list_ends_on_a_short_page(replay):
    page = OneNce().list_sims(Creds(), {})
    assert len(page.items) == 3 and page.cursor is None
    assert replay.calls.count(('GET', '/v1/sims')) == 1


def test_usage_drops_the_total_row_and_sorts(replay):
    days = OneNce().usage(Creds(), {}, ICCID, datetime.date(2026, 1, 1), datetime.date(2026, 1, 3))
    assert [d['date'] for d in days] == ['2026-01-01', '2026-01-02']


def test_quota_reads_remaining_not_used(replay):
    quota = OneNce().quota(Creds(), {}, ICCID, 'data')
    assert (quota['remaining'], quota['used'], quota['usedPercent']) == (400.0, 100.0, 20.0)


def test_a_change_sends_the_whole_record(replay):
    OneNce().set_state(Creds(), {}, ICCID, False)
    sent = [kw['json'] for method, _, kw in replay.sent if method == 'PUT']
    assert sent == [{'status': 'Disabled', 'imei_lock': False, 'label': 'rooftop'}]


def test_topups_are_off_by_default(monkeypatch):
    monkeypatch.delenv('CELLULAR_TOPUPS_ENABLED', raising=False)
    with Replay({}, base_url=API_BASE) as replay:
        with pytest.raises(UpstreamError) as e:
            OneNce().topup(Creds(), {}, ICCID)
    assert e.value.status == 409 and not replay.calls


def test_missing_credentials_are_a_503():
    with pytest.raises(UpstreamError) as e:
        OneNce().check(Creds(client_id='', client_secret=''), {})
    assert e.value.status == 503


def test_a_404_is_not_found(replay):
    replay.recordings[('GET', f'/v1/sims/{ICCID}')] = (404, {})
    with pytest.raises(UpstreamError) as e:
        OneNce().sim(Creds(), {}, ICCID)
    assert e.value.status == 404
