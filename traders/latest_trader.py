"""
Round 3 Trader v5 — Focused: VEV_5000 (BS fair) + VEV_4000 (spread capture)

VEV_5000: Market-make around BS(S, K=5000, T, σ=0.2) fair value.
          Quote inside the best bid/ask, take mispriced orders.

VEV_4000: Keep the v4 spread-based inside quoting that was profitable.

All other products dropped for now.
"""

from datamodel import OrderDepth, TradingState, Order
from typing import List
import json
import math


# ═══════════════ Minimal BS engine (no scipy needed) ═══════════════
_INV_SQRT2 = 1.0 / math.sqrt(2.0)

def _norm_cdf(x):
    return 0.5 * (1.0 + math.erf(x * _INV_SQRT2))

def _norm_pdf(x):
    return math.exp(-0.5 * x * x) / math.sqrt(2.0 * math.pi)

def bs_call(S, K, T, sigma):
    if T <= 0 or sigma <= 0 or S <= 0:
        return max(S - K, 0.0)
    sqrtT = math.sqrt(T)
    d1 = (math.log(S / K) + 0.5 * sigma * sigma * T) / (sigma * sqrtT)
    d2 = d1 - sigma * sqrtT
    return S * _norm_cdf(d1) - K * _norm_cdf(d2)

def bs_delta(S, K, T, sigma):
    if T <= 0 or sigma <= 0 or S <= 0:
        return 1.0 if S > K else 0.0
    sqrtT = math.sqrt(T)
    d1 = (math.log(S / K) + 0.5 * sigma * sigma * T) / (sigma * sqrtT)
    return _norm_cdf(d1)


class Trader:
    # ══════════════ VEV_5000 — BS fair value MM ══════════════
    OPT5000_SIGMA       = 0.2
    OPT5000_MAX_POS     = 300
    OPT5000_TAKE_EDGE   = 0      # take if price crosses fair
    OPT5000_SKEW_PER_LOT = 75

    # ══════════════ VEV_4000 — spread capture (v4 logic) ══════════════
    OPT4000_MAX_POS     = 300
    OPT4000_SKEW_PER_LOT = 75

    # ══════════════ TTE config ══════════════
    # Set before submission: 8 for round's day 0, 7 for day 1, 6 for day 2
    STARTING_TTE = 8
    MAX_TIMESTAMP = 999_900  # last timestamp in a day

    def run(self, state: TradingState):
        result = {}
        s = {}
        if state.traderData:
            try:
                s = json.loads(state.traderData)
            except:
                s = {}

        # Get VE mid price (needed for BS pricing)
        ve_mid = self._get_ve_mid(state)
        if ve_mid is not None:
            s["ve_mid"] = ve_mid
        else:
            ve_mid = s.get("ve_mid")

        # Compute T for this tick
        day_frac = state.timestamp / self.MAX_TIMESTAMP
        T = (self.STARTING_TTE - day_frac) / 252.0

        for product in state.order_depths:
            od = state.order_depths[product]
            pos = state.position.get(product, 0)

            if product == "VEV_5000":
                result[product] = self._trade_5000(od, pos, ve_mid, T)
            elif product == "VEV_4000":
                result[product] = self._trade_spread(product, od, pos,
                                                     self.OPT4000_MAX_POS,
                                                     self.OPT4000_SKEW_PER_LOT)
            else:
                result[product] = []

        return result, 0, json.dumps(s)

    def _get_ve_mid(self, state):
        od = state.order_depths.get("VELVETFRUIT_EXTRACT")
        if od is None or not od.buy_orders or not od.sell_orders:
            return None
        bb = max(od.buy_orders)
        ba = min(od.sell_orders)
        bv = od.buy_orders[bb]
        av = -od.sell_orders[ba]
        t = bv + av
        if t > 0:
            return (bb * av + ba * bv) / t
        return (bb + ba) / 2.0

    # ══════════════ VEV_5000: BS fair value market making ══════════════
    # def _trade_5000(self, od, position, ve_mid, T):
    #     orders = []
    #     product = "VEV_5000"
    #     mx = self.OPT5000_MAX_POS
    #     K = 5000

    #     if not od.buy_orders or not od.sell_orders:
    #         return orders
    #     if ve_mid is None or T <= 0:
    #         return orders

    #     bb = max(od.buy_orders)
    #     ba = min(od.sell_orders)

    #     # BS fair value
    #     fair = bs_call(ve_mid, K, T, self.OPT5000_SIGMA)
    #     fair_int = int(round(fair))

    #     # Inventory skew
    #     skew = 0
    #     if self.OPT5000_SKEW_PER_LOT > 0:
    #         skew = position // self.OPT5000_SKEW_PER_LOT

    #     buy_room = mx - position
    #     sell_room = mx + position

    #     # ── Phase 1: Take mispriced orders ──
    #     edge = self.OPT5000_TAKE_EDGE
    #     pos = position
    #     bought = sold = 0

    #     for ap in sorted(od.sell_orders):
    #         if ap > fair + edge:
    #             break
    #         if pos >= mx:
    #             break
    #         vol = -od.sell_orders[ap]
    #         qty = min(vol, mx - pos)
    #         if qty > 0:
    #             orders.append(Order(product, ap, qty))
    #             pos += qty
    #             bought += qty

    #     for bp in sorted(od.buy_orders, reverse=True):
    #         if bp < fair - edge:
    #             break
    #         if pos <= -mx:
    #             break
    #         vol = od.buy_orders[bp]
    #         qty = min(vol, mx + pos)
    #         if qty > 0:
    #             orders.append(Order(product, bp, -qty))
    #             pos -= qty
    #             sold += qty

    #     # ── Phase 2: Quote inside the spread ──
    #     buy_room -= bought
    #     sell_room -= sold

    #     # Bid: one tick above best bid, but don't bid above fair
    #     our_bid = bb + 1 - skew
    #     our_bid = min(our_bid, fair_int)

    #     # Ask: one tick below best ask, but don't ask below fair
    #     our_ask = ba - 1 - skew
    #     our_ask = max(our_ask, fair_int)

    #     # Ensure we don't cross ourselves
    #     if our_bid >= our_ask:
    #         our_bid = fair_int - 1
    #         our_ask = fair_int + 1

    #     if buy_room > 0 and our_bid > 0:
    #         orders.append(Order(product, our_bid, buy_room))

    #     if sell_room > 0 and our_ask > 0:
    #         orders.append(Order(product, our_ask, -sell_room))

    #     return orders

    
    def _trade_5000(self, od, position, ve_mid, T):
        orders = []
        product = "VEV_5000"
        mx = self.OPT5000_MAX_POS
        K = 5000

        if not od.buy_orders or not od.sell_orders:
            return orders
        if ve_mid is None or T <= 0:
            return orders

        bb = max(od.buy_orders)
        ba = min(od.sell_orders)

        fair = bs_call(ve_mid, K, T, self.OPT5000_SIGMA)
        fair_int = int(round(fair))

        buy_room = mx - position
        sell_room = mx + position

        # Bid inside, but never above fair
        our_bid = min(bb + 1, fair_int)
        # Ask inside, but never below fair
        our_ask = max(ba - 1, fair_int)

        if our_bid >= our_ask:
            our_bid = fair_int - 1
            our_ask = fair_int + 1

        if buy_room > 0:
            orders.append(Order(product, our_bid, buy_room))
        if sell_room > 0:
            orders.append(Order(product, our_ask, -sell_room))

        return orders

    # ══════════════ VEV_4000: Spread-based quoting (v4 logic) ══════════════
    def _trade_spread(self, product, od, position, max_pos, skew_per_lot):
        orders = []
        if not od.buy_orders or not od.sell_orders:
            return orders

        bb = max(od.buy_orders)
        ba = min(od.sell_orders)
        spread = ba - bb

        buy_room = max_pos - position
        sell_room = max_pos + position

        skew = position // skew_per_lot if skew_per_lot > 0 else 0

        if spread >= 4:
            qb = bb + 1 - skew
            qa = ba - 1 - skew
            qb = max(qb, bb + 1)
            qa = min(qa, ba - 1)
            if qb < qa:
                if buy_room > 0:
                    orders.append(Order(product, qb, buy_room))
                if sell_room > 0:
                    orders.append(Order(product, qa, -sell_room))

        elif spread == 3:
            if buy_room > 0:
                orders.append(Order(product, bb + 1, buy_room))
            if sell_room > 0:
                orders.append(Order(product, ba - 1, -sell_room))

        elif spread == 2:
            inside = bb + 1
            if buy_room > 0:
                orders.append(Order(product, inside, buy_room))
            if sell_room > 0:
                orders.append(Order(product, inside, -sell_room))
            if buy_room > 0:
                orders.append(Order(product, bb, buy_room))
            if sell_room > 0:
                orders.append(Order(product, ba, -sell_room))

        elif spread == 1:
            if buy_room > 0:
                orders.append(Order(product, bb, buy_room))
            if sell_room > 0:
                orders.append(Order(product, ba, -sell_room))

        return orders