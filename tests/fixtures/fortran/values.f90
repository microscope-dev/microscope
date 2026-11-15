









module values
  use, intrinsic :: iso_fortran_env, only: output_unit, int8, int16, int32, int64, real32, real64
  use, intrinsic :: iso_c_binding, only: c_intptr_t, c_loc
  implicit none
  private
  public :: scalars, records, descriptors, strings, add
  public :: returns_int, returns_double, returns_logical

  type :: point
    integer(int32) :: x, y
  end type

  type :: segment
    type(point) :: from, to
    integer(int8) :: tag
  end type

  character, parameter :: tab = achar(9)
  integer(c_intptr_t), volatile :: sink = 0

contains


  subroutine reached(checkpoint)

    character(*), intent(in) :: checkpoint
    sink = sink + len(checkpoint)
  end subroutine


  subroutine keep(value)

    type(*), target, intent(inout) :: value(..)
    sink = transfer(c_loc(value), sink)
  end subroutine

  subroutine truth(checkpoint, path, kind, value)
    character(*), intent(in) :: checkpoint, path, kind, value
    write (output_unit, '(A)') 'TRUTH'//tab//checkpoint//tab//path//tab//kind//tab//value
    flush (output_unit)
  end subroutine

  function decimal(value) result(text)
    integer(int64), intent(in) :: value
    character(:), allocatable :: text
    character(24) :: buffer
    write (buffer, '(I0)') value
    text = trim(buffer)
  end function


  function hexadecimal(bits) result(text)
    integer(int64), intent(in) :: bits
    character(:), allocatable :: text
    character(24) :: buffer
    integer :: i
    write (buffer, '(Z0)') bits
    text = '0x'//trim(buffer)
    do i = 3, len(text)
      if (text(i:i) >= 'A' .and. text(i:i) <= 'F') text(i:i) = achar(iachar(text(i:i)) + 32)
    end do
  end function

  function f32_bits(value) result(text)
    real(real32), intent(in) :: value
    character(:), allocatable :: text
    text = hexadecimal(iand(int(transfer(value, 0_int32), int64), int(z'ffffffff', int64)))
  end function

  function f64_bits(value) result(text)
    real(real64), intent(in) :: value
    character(:), allocatable :: text
    text = hexadecimal(transfer(value, 0_int64))
  end function

  function add(a, b) result(total)

    integer, intent(in) :: a, b
    integer, target :: total
    total = a + b
    call keep(total)
  end function

  subroutine scalars()

    integer(int8), target :: small
    integer(int16), target :: wide
    integer(int64), target :: big
    real(real32), target :: single
    real(real64), target :: double
    logical, target :: flag
    complex(real64), target :: z
    character, target :: letter
    small = -5_int8
    wide = 32000_int16
    big = -2_int64**40
    single = 1.5
    double = -0.1_real64
    flag = .true.
    z = (1.5_real64, -2.0_real64)
    letter = 'q'
    call truth('scalars', 'small', 'int', decimal(int(small, int64)))
    call truth('scalars', 'wide', 'int', decimal(int(wide, int64)))
    call truth('scalars', 'big', 'int', decimal(big))
    call truth('scalars', 'single', 'f32', f32_bits(single))
    call truth('scalars', 'double', 'f64', f64_bits(double))
    call truth('scalars', 'flag', 'summary', 'true')
    call truth('scalars', 'z', 'c128', f64_bits(real(z))//':'//f64_bits(aimag(z)))
    call truth('scalars', 'letter', 'string', '"q"')
    call reached('scalars')
    call keep(small); call keep(wide); call keep(big); call keep(single)
    call keep(double); call keep(flag); call keep(z); call keep(letter)
  end subroutine

  subroutine records()

    type(point), target :: origin
    type(segment), target :: line
    integer(int32), target :: numbers(3)
    integer(int32), target :: shifted(-1:1)
    integer(int32), target :: grid(2, 3)
    integer(int32), allocatable, target :: heap(:)
    integer :: i, j
    origin = point(3, -4)
    line = segment(point(1, 2), point(5, 6), 9_int8)
    numbers = [10, 20, 30]
    shifted = [7, 8, 9]
    grid = reshape([11, 21, 12, 22, 13, 23], [2, 3])
    heap = [4, 5]
    call truth('records', 'origin.x', 'int', decimal(int(origin%x, int64)))
    call truth('records', 'origin.y', 'int', decimal(int(origin%y, int64)))
    call truth('records', 'line.to.y', 'int', decimal(int(line%to%y, int64)))
    call truth('records', 'line.tag', 'int', decimal(int(line%tag, int64)))
    do i = 1, 3
      call truth('records', 'numbers.('//decimal(int(i, int64))//')', 'int', &
                 decimal(int(numbers(i), int64)))
    end do
    do i = -1, 1
      call truth('records', 'shifted.('//decimal(int(i, int64))//')', 'int', &
                 decimal(int(shifted(i), int64)))
    end do
    do j = 1, 3
      do i = 1, 2
        call truth('records', 'grid.('//decimal(int(i, int64))//','//decimal(int(j, int64))//')', &
                   'int', decimal(int(grid(i, j), int64)))
      end do
    end do
    do i = 1, 2
      call truth('records', 'heap.('//decimal(int(i, int64))//')', 'int', &
                 decimal(int(heap(i), int64)))
    end do
    call reached('records')
    call keep(origin); call keep(line); call keep(numbers); call keep(shifted); call keep(grid)
    call keep(heap)
  end subroutine


  subroutine descriptors()

    integer(int32), allocatable, target :: table(:, :)
    integer(int32), allocatable, target :: unset(:)
    integer(int32), pointer :: loose(:)
    integer(int32), target :: row(6)
    integer :: i, j
    allocate (table(0:1, -1:1))
    do j = -1, 1
      do i = 0, 1
        table(i, j) = 10*i + j
      end do
    end do
    row = [1, 2, 3, 4, 5, 6]
    loose => null()
    do j = -1, 1
      do i = 0, 1
        call truth('descriptors', 'table.('//decimal(int(i, int64))//','//decimal(int(j, int64))//')', &
                   'int', decimal(int(table(i, j), int64)))
      end do
    end do
    call truth('descriptors', 'unset', 'summary', '<not allocated>')
    call truth('descriptors', 'loose', 'summary', '<not associated>')
    call reached('descriptors')
    call section(row(2::2))
    call keep(table); call keep(unset); call keep(row)
    if (associated(loose)) call keep(loose)
  end subroutine


  subroutine section(part)

    integer(int32), intent(inout), target :: part(:)
    integer :: i
    do i = 1, size(part)
      call truth('section', 'part.('//decimal(int(i, int64))//')', 'int', &
                 decimal(int(part(i), int64)))
    end do
    call reached('section')
    call keep(part)
  end subroutine

  subroutine strings()

    character(len=5), target :: word
    character(:), allocatable, target :: grown
    word = 'hello'
    grown = 'abc'
    grown = grown//f32_bits(1.5)
    call truth('strings', 'word', 'string', '"'//word//'"')
    call truth('strings', '.grown', 'hidden', '')
    call truth('strings', 'grown', 'string', '"'//grown//'"')
    call reached('strings')
    call keep(word); call keep(grown)
  end subroutine



  function returns_int(n) result(value)

    integer, intent(in) :: n
    integer :: value
    value = n * (-11)
    call truth('returned-int', 'returns_int', 'int', decimal(int(value, int64)))
    call reached('returned-int')
  end function

  function returns_double(n) result(value)

    integer, intent(in) :: n
    real(real64) :: value
    value = -6.5_real64 * n
    call truth('returned-double', 'returns_double', 'f64', f64_bits(value))
    call reached('returned-double')
  end function

  function returns_logical(n) result(value)

    integer, intent(in) :: n
    logical :: value
    value = n > 0
    call truth('returned-logical', 'returns_logical', 'summary', 'true')
    call reached('returned-logical')
  end function

end module

program main
  use values
  use, intrinsic :: iso_fortran_env, only: real64
  implicit none
  integer :: n

  n = command_argument_count() + 3
  call scalars()
  call records()
  call descriptors()
  call strings()
  if (add(2, 3) /= 5) error stop 'add'
  if (returns_int(n) /= -11 * n) error stop 'returns_int'
  if (returns_double(n) /= -6.5_real64 * n) error stop 'returns_double'
  if (.not. returns_logical(n)) error stop 'returns_logical'
end program
