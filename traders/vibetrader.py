from datamodel import Order, TradingState
from typing import List, Dict
import json

class Trader:
    POSITION_LIMITS = {
        "ASH_COATED_OSMIUM": 80,
        "INTARIAN_PEPPER_ROOT": 80,
    }

    ASH_FAIR_VALUE = 10000
    PEPPER_AGGRESSIVE_TARGET = 60

    # simple pepper model
    PEPPER_TIMESTAMP_SLOPE = 0.001
    PEPPER_DAY_STEP = 1000
    PEPPER_DAY0_BASE = 12000   # day 0 starts around 12000

    def bid(self):
        return 15

    def run(self, state: TradingState):
        result: Dict[str, List[Order]] = {}

        #result["ASH_COATED_OSMIUM"] = self.trade_ash(state)
        result["INTARIAN_PEPPER_ROOT"] = self.trade_pepper(state)

        traderData = ""
        conversions = 0
        return result, conversions, traderData

    def infer_pepper_day(self, state: TradingState) -> int:
        symbol = "INTARIAN_PEPPER_ROOT"

        if symbol not in state.order_depths:
            return 0

        depth = state.order_depths[symbol]

        buy_orders = dict(sorted(depth.buy_orders.items(), key=lambda x: x[0], reverse=True))
        sell_orders = dict(sorted(depth.sell_orders.items(), key=lambda x: x[0]))

        best_bid = max(buy_orders.keys()) if buy_orders else None
        best_ask = min(sell_orders.keys()) if sell_orders else None

        if best_bid is not None and best_ask is not None:
            observed_price = (best_bid + best_ask) / 2
        elif best_bid is not None:
            observed_price = best_bid
        elif best_ask is not None:
            observed_price = best_ask
        else:
            return 0

        # remove intraday drift, then infer day from the price level
        base_level = observed_price - self.PEPPER_TIMESTAMP_SLOPE * state.timestamp
        inferred_day = int(round((base_level - self.PEPPER_DAY0_BASE) / self.PEPPER_DAY_STEP))
        return inferred_day

    def get_pepper_fair_value(self, state: TradingState) -> int:
        inferred_day = self.infer_pepper_day(state)
        fair = (
            self.PEPPER_DAY0_BASE
            + self.PEPPER_DAY_STEP * inferred_day
            + self.PEPPER_TIMESTAMP_SLOPE * state.timestamp
        )
        return int(round(fair))

    def trade_pepper(self, state: TradingState) -> List[Order]:
        orders: List[Order] = []
        symbol = "INTARIAN_PEPPER_ROOT"

        if symbol not in state.order_depths:
            return orders

        fair_value = self.get_pepper_fair_value(state)

        depth = state.order_depths[symbol]
        pos = state.position.get(symbol, 0)
        limit = self.POSITION_LIMITS[symbol]

        buy_orders = dict(sorted(depth.buy_orders.items(), key=lambda x: x[0], reverse=True))
        sell_orders = dict(sorted(depth.sell_orders.items(), key=lambda x: x[0]))

        best_bid = max(buy_orders.keys()) if buy_orders else None
        best_ask = min(sell_orders.keys()) if sell_orders else None

        max_buy = limit - pos

        # 1) TAKE ALL ASKS BELOW FAIR
        for ask_price, ask_qty in sell_orders.items():
            visible = -ask_qty  # sell quantities are negative

            if max_buy <= 0:
                break

            if ask_price < fair_value:
                qty = min(visible, max_buy)
                if qty > 0:
                    orders.append(Order(symbol, ask_price, qty))
                    pos += qty
                    max_buy -= qty
            else:
                break

        # refresh after taking
        # best_bid = max(buy_orders.keys()) if buy_orders else None
        # best_ask = min(sell_orders.keys()) if sell_orders else None
        max_buy = limit - pos
        max_sell = limit + pos
        # 0) AGGRESSIVE BUY-UP TO +30 INVENTORY
        if pos < self.PEPPER_AGGRESSIVE_TARGET and max_buy > 0:
            aggressive_need = min(self.PEPPER_AGGRESSIVE_TARGET - pos, max_buy)

            for ask_price, ask_qty in sell_orders.items():
                if aggressive_need <= 0:
                    break

                visible = -ask_qty
                qty = min(visible, aggressive_need)

                if qty > 0:
                    orders.append(Order(symbol, ask_price, qty))
                    pos += qty
                    max_buy -= qty
                    aggressive_need -= qty

        # 2) MARKET MAKE ON THE ASK SIDE
        # if best_ask - 1 > fair, sell max possible there, even if not currently long
        if best_ask is not None and max_sell > 0:
            sell_price = best_ask - 1

            # do not cross the bid
            if best_bid is None or sell_price > best_bid:
                if sell_price > fair_value:
                    orders.append(Order(symbol, sell_price, -max_sell))

        # 3) MARKET MAKE ON THE BID SIDE
        # if best_bid + 1 < fair, buy max possible there
        if best_bid is not None and max_buy > 0:
            buy_price = best_bid + 1

            # do not cross the ask
            if best_ask is None or buy_price < best_ask:
                if buy_price < fair_value:
                    orders.append(Order(symbol, buy_price, max_buy))

        return orders