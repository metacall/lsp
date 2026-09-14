from metacall import metacall


def multiply(units, price):
    """Total price for a number of units."""
    return units * price


def total(units, price):
    first = multiply(units, price)
    return metacall('multiply', first, price)
