"""1NCE connector for pulzr-connectivity.

Everything that knows 1NCE's URLs, field names and quirks lives here; callers
only see the normalized shapes this returns. Caching is the driver's, through
the memo it is given, because what is worth remembering is 1NCE's own record
(a SIM's address, a quota block) and the cache holds those as 1NCE sent them.

`creds` is any object with `namespace`, `client_id` and `client_secret`: the
namespace keeps two accounts' tokens and cache entries apart.
"""

import logging
import os
import time
from concurrent.futures import ThreadPoolExecutor

import requests

from pulzr_connectivity import (APN_PROFILE, SIM_ACTIVATE, SIM_INVENTORY, SIM_LOCATION,
                                SIM_LOCK, SIM_RESET, SIM_STATUS, SIM_SUSPEND, SIM_TOPUP,
                                SIM_USAGE, ApnProfile, Capabilities, Driver, Page,
                                UpstreamError)

# Same logger name as before the seam, so log filters and dashboards keep working.
log = logging.getLogger('sim-service')

API_BASE = os.getenv('ONENCE_API_URL', 'https://api.1nce.com/management-api').rstrip('/')
TIMEOUT = float(os.getenv('ONENCE_TIMEOUT', '20'))

# Live attachment state is one call per SIM — 1NCE publishes no bulk form of
# it — so the fleet-wide answer is a fan-out. The per-call ceiling is lower
# than TIMEOUT because the whole sweep has to come back inside net-api's own
# timeout, and one stalled radio must not eat it.
STATUS_WORKERS = int(os.getenv('ONENCE_STATUS_WORKERS', '24'))
STATUS_TIMEOUT = float(os.getenv('ONENCE_STATUS_TIMEOUT', '8'))

# How long 1NCE's own records are worth remembering. See cache.py for why
# there is a cache at all; these are the numbers behind it.
#
# The SIM record is the long one because it is the hot one: it carries the
# address the device UI proxy resolves on every single asset of a page, and an
# address is assigned per SIM and survives re-attachment. The cost is that a
# SIM disabled in 1NCE's own portal reads stale for this long — changes made
# *here* drop it immediately, which covers everything the console can do.
SIM_TTL = float(os.getenv('ONENCE_SIM_TTL', '300'))
LIST_TTL = float(os.getenv('ONENCE_LIST_TTL', '60'))
QUOTA_TTL = float(os.getenv('ONENCE_QUOTA_TTL', '120'))
# Per-day traffic for the usage chart. Yesterday's rows never change and
# today's moves slowly, so this is the one nobody notices being old.
DAILY_TTL = float(os.getenv('ONENCE_DAILY_TTL', '900'))

# Pages of 100 to walk when nobody asked for a particular page. The ceiling is
# a stop, not a limit anyone should reach: an account this size would want a
# different shape entirely, and an unbounded loop against a paginated API is
# how one bad response becomes a hang.
LIST_PAGES = int(os.getenv('ONENCE_LIST_PAGES', '25'))

# Prices change when Parkeze orders more, not otherwise.
PRICES_TTL = float(os.getenv('ONENCE_PRICES_TTL', str(6 * 3600)))
# Pages of 10 (1NCE's ceiling for orders) to walk; a stop, as LIST_PAGES is.
ORDER_PAGES = int(os.getenv('ONENCE_ORDER_PAGES', '50'))
SIM_ORDERS = ('FIRST_ORDER', 'ADDITIONAL_ORDER')
TOPUP_ORDERS = ('TOPUP', 'BULK_TOPUP')
# A lifetime SIM's term when its product does not say: ten years.
DEFAULT_TERM_MONTHS = 120

# 1NCE's APN, which is what gateways have always defaulted to.
APN = 'iot.1nce.net'


def _text(value):
    """1NCE returns some fields as {id, description} and some as a bare string."""
    return value.get('description') if isinstance(value, dict) else value


def _sim(raw):
    """One SIM, in the names the console uses.

    Everything 1NCE returns is carried through. The earlier version kept four
    fields, which meant the console could not answer the questions an operator
    actually arrives with — is it online, what address is it on, why is it
    quiet — without a second trip to 1NCE's own portal.
    """
    return {
        'iccid': raw.get('iccid'),
        'status': _text(raw.get('status')),
        'activatedAt': raw.get('activation_date'),
        'currentQuotaMb': raw.get('current_quota'),
        'quotaStatus': _text(raw.get('quota_status')),
        'smsRemaining': raw.get('current_quota_SMS'),
        'smsStatus': _text(raw.get('quota_status_SMS')),
        # The address the SIM holds on the cellular network. This is what makes
        # a gateway reachable over the tunnel at all, so it is the single most
        # useful field here and it was the one being dropped.
        'ipAddress': raw.get('ip_address'),
        'imsi': raw.get('imsi'),
        'msisdn': raw.get('msisdn'),
        # 1NCE sends the string "null" rather than JSON null for an unset IMEI.
        'imei': None if raw.get('imei') in (None, 'null') else raw.get('imei'),
        'imeiLock': raw.get('imei_lock'),
        # 1NCE's own label for the SIM. Often the only place anybody wrote down
        # what the thing on the end of it is.
        'label': raw.get('label'),
    }


def _quota(raw):
    """A 1NCE quota block.

    `volume` is what is *left* and `total_volume` is what was bought — there is
    no `remaining_volume`, which the previous reading assumed. Reading it that
    way made remaining None and used = total, so every SIM was reported as
    100% consumed no matter how much it had actually sent.
    """
    remaining = raw.get('volume')
    total = raw.get('total_volume')
    used = None
    if isinstance(remaining, (int, float)) and isinstance(total, (int, float)):
        used = round(total - remaining, 3)
    return {
        'remaining': remaining,
        'total': total,
        'used': used,
        'usedPercent': round(used / total * 100, 1) if used is not None and total else None,
        'expiryDate': raw.get('expiry_date'),
        'lastAdded': raw.get('last_volume_added'),
        'lastChangedAt': raw.get('last_status_change_date'),
        'thresholdPercent': raw.get('threshold_percentage'),
        'peakThroughput': raw.get('peak_throughput'),
    }


def _connection(status):
    """The live attachment, flattened out of /status.

    A SIM that is ONLINE carries a whole session underneath: which operator it
    is camped on, over what radio, on which cell, and for how long. All of it
    is nested three or four levels deep in 1NCE's response and none of it was
    reaching the console — which meant "is this gateway on a good link" was not
    answerable here at all.

    An OFFLINE SIM has none of it, so every field is optional by design.
    """
    if not isinstance(status, dict):
        return None

    location = status.get('location') or {}
    pdp = status.get('pdp_context') or {}
    operator = location.get('operator') or {}
    country = operator.get('country') or location.get('country') or {}
    rat = pdp.get('rat_type') or {}

    return {
        'state': status.get('status'),
        'services': status.get('services') or [],
        'operator': operator.get('name'),
        'country': country.get('name'),
        'countryCode': (country.get('iso_code') or '').upper() or None,
        # 2G/3G/4G/NB-IoT. A gateway that has fallen back to 2G is not broken
        # but is about to behave as though it is.
        'radio': rat.get('description'),
        # How long the current data session has been up, as 1NCE reports it.
        'sessionFor': pdp.get('duration'),
        'sessionSince': pdp.get('created'),
        # Enough to identify the tower, which is what distinguishes "moved" from
        # "same place, worse signal".
        'cellId': pdp.get('ci') or None,
        'lac': pdp.get('lac') or None,
        'mcc': pdp.get('mcc') or country.get('mcc'),
        'mnc': pdp.get('mnc'),
        # The address the device holds *in this session*, which can differ from
        # the SIM record if it has re-attached.
        'sessionIp': pdp.get('ue_ip_address'),
        # IMEISV identifies the hardware actually holding the SIM right now.
        'imeisv': pdp.get('imeisv'),
        'lastSeen': location.get('last_updated'),
        'lastSeenGprs': location.get('last_updated_gprs'),
    }


def _cell(connectivity):
    """The serving cell, when 1NCE managed to ask the network for it."""
    info = (connectivity or {}).get('subscriber_info') or {}
    if not info.get('success'):
        return {'available': False, 'reason': info.get('error') or info.get('state')}
    location = info.get('location') or {}
    cgi = location.get('cell_global_id') or {}
    return {
        'available': True,
        'mcc': cgi.get('mcc'),
        'mnc': cgi.get('mnc'),
        'lac': cgi.get('lac'),
        'cellId': cgi.get('cid'),
        'ageSeconds': location.get('age_of_location'),
        'state': info.get('state'),
    }


def _amount(value):
    try:
        return float(value) if value not in (None, '') else None
    except (TypeError, ValueError):
        return None


class OneNce(Driver):
    name = '1nce'
    kind = 'cellular'
    # sim.sms and the OpenVPN network.* capabilities are 1NCE's too, but
    # nothing here implements them yet, and a driver claims only what it does.
    capabilities = Capabilities.of(
        SIM_INVENTORY, SIM_STATUS, SIM_USAGE, SIM_SUSPEND, SIM_ACTIVATE, SIM_TOPUP,
        SIM_RESET, SIM_LOCK, SIM_LOCATION, APN_PROFILE)

    def __init__(self, memo=None, call=None, token=None):
        super().__init__(memo)
        # One OAuth token per account: a sub-organization's API user sees that
        # sub-organization's SIMs and nothing else, which is the point of
        # having one.
        self._tokens = {}
        # The two network edges are overridable because the service's own
        # tests replace them on app.py, and its routes hand them in here.
        self._call = call or self.http
        self._token = token or self.fetch_token

    # ── transport ─────────────────────────────────────────────────────────

    def fetch_token(self, creds):
        """OAuth2 client credentials, cached per account until shortly before expiry."""
        held = self._tokens.get(creds.namespace)
        if held and time.time() < held['expires_at']:
            return held['value']

        if not creds.client_id or not creds.client_secret:
            raise UpstreamError('1NCE credentials are not configured', status=503)

        response = requests.post(
            f'{API_BASE}/oauth/token',
            json={'grant_type': 'client_credentials'},
            auth=(creds.client_id, creds.client_secret),
            timeout=TIMEOUT,
        )
        if response.status_code >= 400:
            raise UpstreamError(f'1NCE auth failed: {response.status_code}', status=502)

        payload = response.json()
        # Refresh a minute early so a call never races the expiry.
        self._tokens[creds.namespace] = {
            'value': payload['access_token'],
            'expires_at': time.time() + int(payload.get('expires_in', 3600)) - 60,
        }
        return payload['access_token']

    def http(self, method, path, account, **kwargs):
        # Overridable, because the live-network calls need a tighter ceiling than
        # the ones that only read 1NCE's own records.
        kwargs.setdefault('timeout', TIMEOUT)
        try:
            response = requests.request(
                method,
                f'{API_BASE}{path}',
                headers={'Authorization': f'Bearer {self._token(account)}'},
                **kwargs,
            )
        except requests.RequestException as e:
            raise UpstreamError(f'1NCE unreachable: {e}')

        if response.status_code == 404:
            raise UpstreamError('Not found in 1NCE', status=404)
        if response.status_code >= 400:
            raise UpstreamError(f'1NCE error {response.status_code}: {response.text[:200]}')
        return response.json() if response.content else {}

    def _get(self, creds, path, **kwargs):
        return self._call('GET', path, account=creds, **kwargs)

    def forget_token(self, creds):
        self._tokens.pop(creds.namespace, None)

    # ── the interface ─────────────────────────────────────────────────────

    def check(self, creds, config):
        """Proof the credentials work: a fresh token from 1NCE, not a held one."""
        self.forget_token(creds)
        self._token(creds)
        return self.capabilities

    def _all_sims(self, creds):
        """Every SIM on the account, following 1NCE's pagination to the end.

        100 is their page ceiling, and asking for page 1 was treated as asking
        for the account. It is not: SIM 101 onwards simply did not exist as far
        as anything here was concerned — missing from the SIM list, unmatchable
        when a gateway reported its ICCID, and reported as orphaned when one was
        bound to it. Nothing said so, because a short page and a last page look
        the same until you ask for the next one.
        """
        entries = []
        for page in range(1, LIST_PAGES + 1):
            batch = self._get(creds, '/v1/sims', params={'page': page, 'pageSize': 100}) or []
            entries.extend(batch)
            if len(batch) < 100:
                return entries
        log.warning('stopped walking the SIM list at %s pages', LIST_PAGES)
        return entries

    def list_sims(self, creds, config, cursor=None, page_size=100):
        # An explicit page is answered as asked and not cached: that is somebody
        # looking at 1NCE's pagination itself, which is a different question
        # from "what is on the account".
        if cursor:
            result = self._get(creds, '/v1/sims', params={
                'page': cursor, 'pageSize': min(page_size, 100)})
            return Page([_sim(entry) for entry in (result or [])])
        raw = self.memo.fetch(creds, 'list', LIST_TTL, lambda: self._all_sims(creds))
        return Page([_sim(entry) for entry in raw])

    def _record(self, creds, iccid):
        return self.memo.fetch(creds, f'rec:{iccid}', SIM_TTL,
                               lambda: self._get(creds, f'/v1/sims/{iccid}'))

    def sim(self, creds, config, iccid):
        return _sim(self._record(creds, iccid))

    def forget(self, creds, iccid, sweep=False):
        """Drop everything remembered about one SIM, and the list it appears in.

        Called after anything that changes a SIM here. What it deliberately does
        not do is try to patch the cached values to what they should now be:
        1NCE is the record, and a cache rewritten from our guess at the outcome
        is a cache that can be wrong in a way re-reading never is.

        `sweep` is for the two things that take a SIM off the network —
        disabling it, and resetting it — because the live-status sweep is the
        one cached value shared by the whole account, and re-earning it is a
        call per SIM. Renaming a SIM does not change whether any of them is
        attached, so a rename must not cost that. (`status` is the key app.py
        keeps the sweep under.)
        """
        self.memo.drop(creds, f'rec:{iccid}', f'quota:{iccid}:data', f'quota:{iccid}:sms',
                       'list', *(['status'] if sweep else []))

    def update(self, creds, config, iccid, changes):
        """Status, IMEI lock and label: the three things 1NCE lets us change.

        PUT, not PATCH. 1NCE answers PATCH on a SIM with 405 Method Not Allowed,
        which is why every rename from the console failed with a 502.

        And the full record, not just the changed keys. A PUT carrying only a
        label could not be shown to leave status alone — the only SIM available
        to test on was already Enabled and unlocked, so "kept" and "reset to the
        default" looked the same. If it resets, renaming a disabled SIM would
        quietly switch it back on and start billing it. Reading first and
        sending everything makes that impossible, at the cost of a race with a
        change made in 1NCE's own portal in the same second.

        Read live, not from the memo: this read is the basis of a write, and a
        record up to SIM_TTL old sent back as a PUT would quietly re-assert a
        status somebody changed in between — the one place where being a few
        minutes stale stops being a display problem.
        """
        current = self._get(creds, f'/v1/sims/{iccid}')
        record = {
            'status': _text(current.get('status')),
            'imei_lock': current.get('imei_lock'),
            'label': current.get('label') or '',
        }
        record.update(changes)
        self._call('PUT', f'/v1/sims/{iccid}', account=creds, json=record)
        log.info('sim %s changed: %s', iccid, changes)
        # Everything remembered about this SIM is now a statement about the
        # past, and the live sweep too when a status changed — disabling a SIM
        # takes it off the network, and the sweep is what somebody is watching.
        self.forget(creds, iccid, sweep='status' in changes)
        # Re-read rather than trust the response: 1NCE answers a PUT with an
        # empty body, and the console needs the row it should now be showing.
        return _sim(self._get(creds, f'/v1/sims/{iccid}'))

    def set_state(self, creds, config, iccid, active):
        return self.update(creds, config, iccid, {'status': 'Enabled' if active else 'Disabled'})

    def quota(self, creds, config, iccid, kind):
        """One quota block, remembered briefly.

        An allowance moves in kilobytes per minute on a backup link that is
        idle most of the time, so this is the cheapest thing on the account to
        cache and the one asked for most — the fleet health strip wants it for
        every gateway at once.
        """
        raw = self.memo.fetch(creds, f'quota:{iccid}:{kind}', QUOTA_TTL,
                              lambda: self._get(creds, f'/v1/sims/{iccid}/quota/{kind}'))
        return _quota(raw)

    def usage(self, creds, config, iccid, start, end):
        """Per-day traffic, oldest first, for the usage chart.

        The date range is not optional. Without start_dt/end_dt 1NCE returns
        *today only* — one data point, which draws as an empty chart and reads
        as "this SIM has never sent anything". With a range it returns a row
        per day.

        1NCE also returns a TOTAL pseudo-row in the same list as the dated
        ones; it is dropped rather than plotted as a day, which is what it looks
        like to any caller that does not know to expect it.
        """
        def produce():
            stats = (self._get(creds, f'/v1/sims/{iccid}/usage', params={
                'start_dt': start.isoformat(), 'end_dt': end.isoformat(),
            }) or {}).get('stats') or []
            out = []
            for row in stats:
                if row.get('date') == 'TOTAL':
                    continue
                data, sms = row.get('data') or {}, row.get('sms') or {}
                out.append({
                    'date': row.get('date'),
                    'mb': float(data.get('volume') or 0),
                    'txMb': float(data.get('volume_tx') or 0),
                    'rxMb': float(data.get('volume_rx') or 0),
                    'cost': float(data.get('cost') or 0),
                    'sms': float(sms.get('volume') or 0),
                })
            return sorted(out, key=lambda r: r['date'] or '')

        return self.memo.fetch(creds, f'daily:{iccid}:{(end - start).days}', DAILY_TTL, produce)

    def topup(self, creds, config, iccid, mb=None):
        """Buy a SIM more data. A real order, charged to the account, so it stays
        off until CELLULAR_TOPUPS_ENABLED and is never called in a test.

        1NCE documents no body, and `payment_method` falling back to banktransfer
        when it is absent, so the method is stated rather than left to that. The
        amount is the product's, not ours: `mb` is not sent.
        """
        if os.getenv('CELLULAR_TOPUPS_ENABLED', 'false').lower() != 'true':
            raise UpstreamError('Top-ups are off', status=409)
        method = os.environ['ONENCE_TOPUP_PAYMENT_METHOD']
        self._call('POST', f'/v1/sims/{iccid}/topup?payment_method={method}', account=creds)

    def reset(self, creds, config, iccid):
        """Force a network re-attach. The remote equivalent of a power cycle."""
        self._call('POST', f'/v1/sims/{iccid}/reset', account=creds)
        log.info('connectivity reset requested for %s', iccid)
        # The SIM is about to detach and re-attach, so the cached sweep — and
        # the address on the record — are statements about the past.
        self.forget(creds, iccid, sweep=True)

    def apn_profile(self, config):
        return ApnProfile(apn=APN)

    # ── live network state ────────────────────────────────────────────────

    def statuses(self, creds, config, iccids):
        """Each SIM's attachment, by ICCID, one call per SIM.

        A SIM whose own call fails lands as None — unknown — never as
        "OFFLINE". "We could not ask" and "it is not attached" send an operator
        to different places, and collapsing them is how a working site gets
        driven to.
        """
        # Warmed once, on this thread. Two dozen workers arriving at an empty
        # token cache together would otherwise each fetch their own.
        self._token(creds)

        def state(iccid):
            try:
                body = self._call('GET', f'/v1/sims/{iccid}/status', account=creds,
                                  timeout=STATUS_TIMEOUT)
                return (body or {}).get('status')
            except UpstreamError as e:
                log.info('status unavailable for %s: %s', iccid, e.message)
                return None

        with ThreadPoolExecutor(max_workers=STATUS_WORKERS) as pool:
            return dict(zip(iccids, pool.map(state, iccids)))

    def connection(self, creds, config, iccid):
        return _connection(self._get(creds, f'/v1/sims/{iccid}/status'))

    def cell(self, creds, config, iccid):
        return _cell(self._get(creds, f'/v1/sims/{iccid}/connectivity_info'))

    def events(self, creds, config, iccid, limit=25):
        """Attach, detach and location updates, newest first.

        This is the closest thing to a log of what the modem has been doing, and
        `alert`/`event_severity` are 1NCE telling us which entries it considers
        worth noticing.
        """
        # No `sort` parameter: 1NCE answers 422 for every spelling of it, and the
        # 422 was being swallowed into an empty list — the events simply looked
        # absent. Ordering happens here instead.
        body = self._get(creds, f'/v1/sims/{iccid}/events', params={'pageSize': limit}) or {}
        events = [
            {
                'at': e.get('timestamp'),
                'type': e.get('event_type'),
                'operator': e.get('operator'),
                'country': e.get('country'),
                'imei': e.get('imei'),
                'alert': bool(e.get('alert')),
                'severity': e.get('event_severity'),
            }
            for e in (body.get('events') or [])
        ]
        return sorted(events, key=lambda e: e['at'] or '', reverse=True)

    # ── what the SIMs cost ────────────────────────────────────────────────
    #
    # For Parkeze staff's direct cost of an organization (net-api's
    # utils/usage_value.py), never for the organization. Read from what was
    # ordered: 1NCE's order history carries the invoice amount and the SIMs each
    # order covered, which is what each SIM and each top-up actually cost.
    #
    # Read only. POST /v1/orders places a real order and is not called anywhere;
    # the one order this service places is a top-up, behind its flag (topup).

    def _orders(self, creds):
        out = []
        for page in range(1, ORDER_PAGES + 1):
            batch = self._get(creds, '/v1/orders',
                              params={'page': page, 'pageSize': 10, 'sort': 'order_date'}) or []
            out.extend(batch)
            if len(batch) < 10:
                return out
        log.warning('stopped walking the order list at %s pages', ORDER_PAGES)
        return out

    def _order_sims(self, creds, order):
        """The ICCIDs an order covered. The list answer may leave them out, so a
        SIM or top-up order without them is read on its own."""
        sims = order.get('sims')
        if not sims and order.get('order_number') is not None:
            number = int(order['order_number'])
            sims = (self._get(creds, f'/v1/orders/{number}') or {}).get('sims')
        return [s['iccid'] for s in sims or [] if s.get('iccid')]

    @staticmethod
    def _units(order, sims):
        """How many SIMs (or top-ups) the invoice amount is for."""
        if sims:
            return len(sims)
        return sum(int(p.get('quantity') or 0) for p in order.get('products') or []) or None

    def _prices(self, creds):
        try:
            products = {p.get('id'): p for p in self._get(creds, '/v1/products') or []}
        except UpstreamError as e:
            log.warning('could not read 1NCE products: %s', e.message)
            products = {}

        sims, topups, sim_orders, topup_orders = {}, {}, [], []
        for order in sorted(self._orders(creds), key=lambda o: o.get('order_date') or ''):
            kind = order.get('order_type')
            if kind not in SIM_ORDERS + TOPUP_ORDERS or order.get('order_status') == 'CANCELLED':
                continue
            iccids = self._order_sims(creds, order)
            units = self._units(order, iccids)
            total = _amount(order.get('invoice_amount'))
            if total is None and kind in SIM_ORDERS:
                # No invoice amount yet: the catalog's one-time charges instead.
                charges = [(_amount(c.get('amount_net')) or 0) * int(p.get('quantity') or 0)
                           for p in order.get('products') or []
                           for c in (products.get(p.get('product_id')) or {}).get('one_time_charges') or []]
                total = sum(charges) if charges else None
            if total is None or not units:
                continue
            terms = [products.get(p.get('product_id'), {}).get('min_contract_duration')
                     for p in order.get('products') or []]
            entry = {
                'orderNumber': str(order.get('order_number')),
                'orderDate': order.get('order_date'),
                'unitPrice': round(total / units, 4),
                'currency': order.get('currency'),
                'termMonths': next((int(t) for t in terms if t), DEFAULT_TERM_MONTHS),
            }
            if kind in SIM_ORDERS:
                sim_orders.append(entry)
                for iccid in iccids:
                    sims[iccid] = entry
            else:
                topup_orders.append(entry)
                for iccid in iccids:
                    topups[iccid] = entry
        return {
            'sims': sims,
            'topups': topups,
            # The latest of each, for a SIM no order names.
            'latestSim': sim_orders[-1] if sim_orders else None,
            'latestTopup': topup_orders[-1] if topup_orders else None,
        }

    def prices(self, creds, config):
        return self.memo.fetch(creds, 'prices', PRICES_TTL, lambda: self._prices(creds))
