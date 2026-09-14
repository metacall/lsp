export function compute_total(units: number, price: number): unknown {
    return metacall("multiply", units, price);
}
