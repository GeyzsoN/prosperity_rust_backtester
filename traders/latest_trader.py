from datamodel import Order, OrderDepth, TradingState
from typing import Dict, List


class Trader:
    LIMITS = {
        "EMERALDS": 80,
        "TOMATOES": 80,
        "INTARIAN_PEPPER_ROOT": 80,
        "ASH_COATED_OSMIUM": 80,
        "HYDROGEL_PACK": 200,
        "VELVETFRUIT_EXTRACT": 200,
        "VEV_4000": 300,
        "VEV_4500": 300,
        "VEV_5000": 300,
        "VEV_5100": 300,
        "VEV_5200": 300,
        "VEV_5300": 300,
        "VEV_5400": 300,
        "VEV_5500": 300,
        "VEV_6000": 300,
        "VEV_6500": 300,
    }
    ROUND5_PREFIXES = (
        "GALAXY_SOUNDS_",
        "SLEEP_POD_",
        "MICROCHIP_",
        "PEBBLES_",
        "ROBOT_",
        "UV_VISOR_",
        "TRANSLATOR_",
        "PANEL_",
        "OXYGEN_SHAKE_",
        "SNACKPACK_",
    )
    ROUND5_LIMIT = 10
    QUOTE_SIZE = 5

    def run(self, state: TradingState):
        orders_by_product: Dict[str, List[Order]] = {}

        for product, order_depth in state.order_depths.items():
            limit = self.limit_for(product)
            if limit is None:
                orders_by_product[product] = []
                continue
            position = int(state.position.get(product, 0))
            orders_by_product[product] = self.quote_both_sides(
                product,
                order_depth,
                position,
                limit,
            )

        return orders_by_product, 0, ""

    def limit_for(self, product: str):
        if product in self.LIMITS:
            return self.LIMITS[product]
        if product.startswith(self.ROUND5_PREFIXES):
            return self.ROUND5_LIMIT
        return None

    def quote_both_sides(
        self,
        product: str,
        order_depth: OrderDepth,
        position: int,
        limit: int,
    ) -> List[Order]:
        if not order_depth.buy_orders or not order_depth.sell_orders:
            return []

        best_bid = max(order_depth.buy_orders)
        best_ask = min(order_depth.sell_orders)
        if best_bid >= best_ask:
            return []

        if best_ask - best_bid > 1:
            bid_price = best_bid + 1
            ask_price = best_ask - 1
        else:
            bid_price = best_bid
            ask_price = best_ask

        buy_size = min(self.QUOTE_SIZE, max(0, limit - position))
        sell_size = min(self.QUOTE_SIZE, max(0, limit + position))

        orders: List[Order] = []
        if buy_size > 0:
            orders.append(Order(product, bid_price, buy_size))
        if sell_size > 0:
            orders.append(Order(product, ask_price, -sell_size))
        return orders
