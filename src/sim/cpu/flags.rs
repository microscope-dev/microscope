use super::{ADJUST, CARRY, OVERFLOW, PARITY, SIGN, STATUS_FLAGS, ZERO};

pub const fn mask(bits: usize) -> u64 {
    if bits >= 64 {
        u64::MAX
    } else {
        (1 << bits) - 1
    }
}

const fn sign_bit(bits: usize) -> u64 {
    1 << (bits - 1)
}

pub const fn sign_extend(value: u64, bits: usize) -> u64 {
    let shift = 64 - bits;
    (((value << shift).cast_signed()) >> shift).cast_unsigned()
}

pub const fn sign_zero_parity(result: u64, bits: usize) -> u64 {
    let result = result & mask(bits);
    let mut flags = 0;
    if result & sign_bit(bits) != 0 {
        flags |= SIGN;
    }

    if result == 0 {
        flags |= ZERO;
    }

    if (result & 0xff).count_ones().is_multiple_of(2) {
        flags |= PARITY;
    }

    flags
}

pub const fn with_status(rflags: u64, status: u64) -> u64 {
    (rflags & !STATUS_FLAGS) | status
}

pub const fn add(a: u64, b: u64, carry: bool, bits: usize) -> (u64, u64) {
    let (a, b) = (a & mask(bits), b & mask(bits));
    let wide = a as u128 + b as u128 + carry as u128;
    #[expect(
        clippy::cast_possible_truncation,
        reason = "masked to the operand width"
    )]
    let result = (wide as u64) & mask(bits);

    let mut flags = sign_zero_parity(result, bits);
    if wide >> bits != 0 {
        flags |= CARRY;
    }

    if (a ^ result) & (b ^ result) & sign_bit(bits) != 0 {
        flags |= OVERFLOW;
    }

    if (a ^ b ^ result) & 0x10 != 0 {
        flags |= ADJUST;
    }

    (result, flags)
}

pub const fn sub(a: u64, b: u64, borrow: bool, bits: usize) -> (u64, u64) {
    let (a, b) = (a & mask(bits), b & mask(bits));
    let result = a.wrapping_sub(b).wrapping_sub(borrow as u64) & mask(bits);

    let mut flags = sign_zero_parity(result, bits);
    if (a as u128) < b as u128 + borrow as u128 {
        flags |= CARRY;
    }

    if (a ^ b) & (a ^ result) & sign_bit(bits) != 0 {
        flags |= OVERFLOW;
    }

    if (a ^ b ^ result) & 0x10 != 0 {
        flags |= ADJUST;
    }

    (result, flags)
}

pub const fn logic(result: u64, bits: usize) -> u64 {
    sign_zero_parity(result, bits)
}

pub const fn shift_left(value: u64, count: u32, bits: usize) -> (u64, u64) {
    let value = value & mask(bits);
    let result = if count as usize >= 64 {
        0
    } else {
        (value << count) & mask(bits)
    };

    let mut flags = sign_zero_parity(result, bits);
    let carried = count as usize <= bits && (value >> (bits - count as usize)) & 1 != 0;
    if carried {
        flags |= CARRY;
    }

    if ((result & sign_bit(bits) != 0) != carried) && count == 1 {
        flags |= OVERFLOW;
    }

    (result, flags)
}

pub const fn shift_right(value: u64, count: u32, bits: usize) -> (u64, u64) {
    let value = value & mask(bits);
    let result = if count >= 64 { 0 } else { value >> count };
    let mut flags = sign_zero_parity(result, bits);

    if count as usize <= bits && (value >> (count - 1)) & 1 != 0 {
        flags |= CARRY;
    }

    if count == 1 && value & sign_bit(bits) != 0 {
        flags |= OVERFLOW;
    }

    (result, flags)
}

pub const fn shift_left_double(
    destination: u64,
    source: u64,
    count: u32,
    bits: usize,
) -> (u64, u64) {
    let (destination, source) = (destination & mask(bits), source & mask(bits));
    let result = ((destination << count) | (source >> (bits - count as usize))) & mask(bits);
    let mut flags = sign_zero_parity(result, bits);

    if (destination >> (bits - count as usize)) & 1 != 0 {
        flags |= CARRY;
    }

    if count == 1 && (result ^ destination) & sign_bit(bits) != 0 {
        flags |= OVERFLOW;
    }

    (result, flags)
}

pub const fn multiply_overflow(overflowed: bool) -> u64 {
    if overflowed { CARRY | OVERFLOW } else { 0 }
}

pub const fn condition(code: iced_x86::ConditionCode, rflags: u64) -> Option<bool> {
    use iced_x86::ConditionCode as Code;
    let carry = rflags & CARRY != 0;
    let zero = rflags & ZERO != 0;
    let sign = rflags & SIGN != 0;
    let overflow = rflags & OVERFLOW != 0;
    let parity = rflags & PARITY != 0;

    Some(match code {
        Code::o => overflow,
        Code::no => !overflow,
        Code::b => carry,
        Code::ae => !carry,
        Code::e => zero,
        Code::ne => !zero,
        Code::be => carry || zero,
        Code::a => !carry && !zero,
        Code::s => sign,
        Code::ns => !sign,
        Code::p => parity,
        Code::np => !parity,
        Code::l => sign != overflow,
        Code::ge => sign == overflow,
        Code::le => zero || sign != overflow,
        Code::g => !zero && sign == overflow,
        Code::None => return None,
    })
}
