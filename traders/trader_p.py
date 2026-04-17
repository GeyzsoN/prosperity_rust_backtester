from datamodel import Order, TradingState
from typing import List, Dict
import json


class Trader:
    POSITION_LIMITS = {
        "ASH_COATED_OSMIUM": 80,
        "INTARIAN_PEPPER_ROOT": 80,
    }

    ASH_FAIR_VALUE = 10000

    # =========================
    # PEPPER TREND FIT
    # from your regression
    # =========================
    PEPPER_K_CONTINUOUS = 0.001000006003413
    PEPPER_B_CONTINUOUS = 9999.987095180174947

    PEPPER_TIMESTAMP_COEF = 0.001000006003413
    PEPPER_DAY_COEF = 1000.006003413332678
    PEPPER_INTERCEPT = 11999.999102006840985

    STEP_SIZE = 100
    DAY_SPAN = 1_000_000
    DAY_MIN = -2

    # IMPORTANT:
    # if live trading starts after training days -2,-1,0,
    # then the next unseen day is day = 1
    PEPPER_START_DAY = 1

    def bid(self):
        return 15

    # -------------------------
    # traderData helpers
    # -------------------------
    def load_memory(self, traderData: str) -> dict:
        if traderData:
            try:
                return json.loads(traderData)
            except Exception:
                pass
        return {
            "pepper_day": self.PEPPER_START_DAY,
            "last_timestamp": None,
        }

    def update_memory_day(self, timestamp: int, memory: dict) -> dict:
        last_timestamp = memory.get("last_timestamp")
        pepper_day = memory.get("pepper_day", self.PEPPER_START_DAY)

        # timestamp reset means new day
        if last_timestamp is not None and timestamp < last_timestamp:
            pepper_day += 1

        memory["last_timestamp"] = timestamp
        memory["pepper_day"] = pepper_day
        return memory

    # -------------------------
    # fair value helpers
    # -------------------------
    def get_pepper_fair_value(self, day: int, timestamp: int) -> int:
        fair = (
            self.PEPPER_TIMESTAMP_COEF * timestamp
            + self.PEPPER_DAY_COEF * day
            + self.PEPPER_INTERCEPT
        )
        return int(round(fair))

    # equivalent version using continuous timestamp
    def get_pepper_fair_value_continuous(self, day: int, timestamp: int) -> int:
        continuous_timestamp = (day - self.DAY_MIN) * self.DAY_SPAN + timestamp
        fair = self.PEPPER_K_CONTINUOUS * continuous_timestamp + self.PEPPER_B_CONTINUOUS
        return int(round(fair))

    def run(self, state: TradingState):
        result: Dict[str, List[Order]] = {}

        memory = self.load_memory(state.traderData)
        memory = self.update_memory_day(state.timestamp, memory)

        result["ASH_COATED_OSMIUM"] = self.trade_ash(state)
        result["INTARIAN_PEPPER_ROOT"] = self.trade_pepper(state, memory["pepper_day"])

        traderData = json.dumps(memory)
        conversions = 0
        return result, conversions, traderData

    def trade_ash(self, state: TradingState) -> List[Order]:
        orders: List[Order] = []
        symbol = "ASH_COATED_OSMIUM"
        fair_value = 10000

        if symbol not in state.order_depths:
            return orders

        depth = state.order_depths[symbol]
        pos = state.position.get(symbol, 0)
        limit = self.POSITION_LIMITS[symbol]

        buy_orders = dict(sorted(depth.buy_orders.items(), key=lambda x: x[0], reverse=True))
        sell_orders = dict(sorted(depth.sell_orders.items(), key=lambda x: x[0]))

        max_buy = limit - pos
        max_sell = limit + pos

        # 1. TAKING
        for ask_price, ask_qty in sell_orders.items():
            visible = -ask_qty
            if max_buy <= 0:
                break

            if ask_price <= fair_value - 1:
                qty = min(visible, max_buy)
                if qty > 0:
                    orders.append(Order(symbol, ask_price, qty))
                    max_buy -= qty
                    pos += qty

            elif ask_price <= fair_value and pos < 0:
                qty = min(visible, -pos, max_buy)
                if qty > 0:
                    orders.append(Order(symbol, ask_price, qty))
                    max_buy -= qty
                    pos += qty

        for bid_price, bid_qty in buy_orders.items():
            if max_sell <= 0:
                break

            if bid_price >= fair_value + 1:
                qty = min(bid_qty, max_sell)
                if qty > 0:
                    orders.append(Order(symbol, bid_price, -qty))
                    max_sell -= qty
                    pos -= qty

            elif bid_price >= fair_value and pos > 0:
                qty = min(bid_qty, pos, max_sell)
                if qty > 0:
                    orders.append(Order(symbol, bid_price, -qty))
                    max_sell -= qty
                    pos -= qty

        # 2. MAKING
        if buy_orders and sell_orders:
            bid_wall = min(buy_orders.keys())
            ask_wall = max(sell_orders.keys())
            wall_mid = (bid_wall + ask_wall) / 2

            bid_price = int(bid_wall + 1)
            ask_price = int(ask_wall - 1)

            for bp, bv in buy_orders.items():
                overbid_price = bp + 1
                if bv > 1 and overbid_price < wall_mid:
                    bid_price = max(bid_price, overbid_price)
                    break
                elif bp < wall_mid:
                    bid_price = max(bid_price, bp)
                    break

            for sp, sv in sell_orders.items():
                undercut_price = sp - 1
                if abs(sv) > 1 and undercut_price > wall_mid:
                    ask_price = min(ask_price, undercut_price)
                    break
                elif sp > wall_mid:
                    ask_price = min(ask_price, sp)
                    break

            if max_buy > 0:
                orders.append(Order(symbol, bid_price, max_buy))
            if max_sell > 0:
                orders.append(Order(symbol, ask_price, -max_sell))

        return orders

    def trade_pepper(self, state: TradingState, pepper_day: int) -> List[Order]:
        orders: List[Order] = []
        symbol = "INTARIAN_PEPPER_ROOT"

        if symbol not in state.order_depths:
            return orders

        fair_value = self.get_pepper_fair_value(pepper_day, state.timestamp)

        depth = state.order_depths[symbol]
        pos = state.position.get(symbol, 0)
        limit = self.POSITION_LIMITS[symbol]

        buy_orders = dict(sorted(depth.buy_orders.items(), key=lambda x: x[0], reverse=True))
        sell_orders = dict(sorted(depth.sell_orders.items(), key=lambda x: x[0]))

        max_buy = limit - pos

        best_bid = max(buy_orders.keys()) if buy_orders else None
        best_ask = min(sell_orders.keys()) if sell_orders else None

        # ---------------------------------
        # 1) IMMEDIATELY BUY CHEAP ASKS
        # ---------------------------------
        # Since trend is assumed to always hold, keep refilling inventory
        # whenever someone is willing to sell below fair.
        for ask_price, ask_qty in sell_orders.items():
            visible = -ask_qty  # raw sell qty is negative

            if max_buy <= 0:
                break

            if ask_price < fair_value:
                qty = min(visible, max_buy)
                if qty > 0:
                    orders.append(Order(symbol, ask_price, qty))
                    pos += qty
                    max_buy -= qty
            else:
                # asks are sorted ascending, so once we hit >= fair we stop
                break

        # refresh after taking
        best_bid = max(buy_orders.keys()) if buy_orders else None
        best_ask = min(sell_orders.keys()) if sell_orders else None

        # ---------------------------------
        # 2) PASSIVE RELOAD BID TO STAY LONG
        # ---------------------------------
        # If we are not yet full, leave a bid below fair so inventory comes back.
        if max_buy > 0:
            if best_bid is None and best_ask is None:
                reload_bid = fair_value - 1
            elif best_bid is None:
                reload_bid = min(fair_value - 1, best_ask - 1)
            elif best_ask is None:
                reload_bid = min(fair_value - 1, best_bid + 1)
            else:
                reload_bid = min(fair_value - 1, best_bid + 1)
                if reload_bid >= best_ask:
                    reload_bid = best_ask - 1

            if reload_bid < fair_value:
                orders.append(Order(symbol, int(reload_bid), max_buy))

        # ---------------------------------
        # 3) SELL CURRENT INVENTORY RICH
        # ---------------------------------
        # Post the inventory at best_ask - 1, but ONLY if that is still above fair.
        # Sell only what we currently own; do not go short.
        if pos > 0 and best_ask is not None:
            sell_price = best_ask - 1

            # never cross into the bid by mistake
            if best_bid is not None and sell_price <= best_bid:
                sell_price = best_ask

            if sell_price > fair_value:
                orders.append(Order(symbol, int(sell_price), -pos))

        return orders