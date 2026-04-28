from datamodel import Order, OrderDepth, TradingState
from typing import Dict, List


class Trader:
    LIMITS = {
        "EMERALDS": 80,
        "TOMATOES": 80,
        "INTARIAN_PEPPER_ROOT": 80,
        "ASH_COATED_OSMIUM": 80,
        "RAINFOREST_RESIN": 50,
        "KELP": 50,
        "SQUID_INK": 50,
        "CROISSANTS": 250,
        "JAMS": 350,
        "DJEMBES": 60,
        "PICNIC_BASKET1": 60,
        "PICNIC_BASKET2": 100,
        "VOLCANIC_ROCK": 400,
        "VOLCANIC_ROCK_VOUCHER_9500": 200,
        "VOLCANIC_ROCK_VOUCHER_9750": 200,
        "VOLCANIC_ROCK_VOUCHER_10000": 200,
        "VOLCANIC_ROCK_VOUCHER_10250": 200,
        "VOLCANIC_ROCK_VOUCHER_10500": 200,
        "MAGNIFICENT_MACARONS": 75,
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
    DEFAULT_LIMIT = 100
    TEST_SIZE = 1

    def run(self, state: TradingState):
        orders_by_product: Dict[str, List[Order]] = {}

        for product, order_depth in state.order_depths.items():
            position = int(state.position.get(product, 0))
            limit = self.limit_for(product)
            orders_by_product[product] = self.cross_visible_book(
                product,
                order_depth,
                position,
                limit,
            )

        return orders_by_product, 0, ""

    def limit_for(self, product: str) -> int:
        if product in self.LIMITS:
            return self.LIMITS[product]
        if product.startswith(self.ROUND5_PREFIXES):
            return self.ROUND5_LIMIT
        return self.DEFAULT_LIMIT

    def cross_visible_book(
        self,
        product: str,
        order_depth: OrderDepth,
        position: int,
        limit: int,
    ) -> List[Order]:
        orders: List[Order] = []

        if order_depth.sell_orders and position < limit:
            best_ask = min(order_depth.sell_orders)
            ask_volume = max(0, -int(order_depth.sell_orders[best_ask]))
            buy_size = min(self.TEST_SIZE, ask_volume, limit - position)
            if buy_size > 0:
                orders.append(Order(product, best_ask, buy_size))

        if order_depth.buy_orders and position > -limit:
            best_bid = max(order_depth.buy_orders)
            bid_volume = max(0, int(order_depth.buy_orders[best_bid]))
            sell_size = min(self.TEST_SIZE, bid_volume, limit + position)
            if sell_size > 0:
                orders.append(Order(product, best_bid, -sell_size))

        return orders
