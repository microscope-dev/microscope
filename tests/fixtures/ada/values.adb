










with Ada.Characters.Latin_1;
with Ada.Command_Line;
with Ada.Strings.Fixed;
with Ada.Strings.Unbounded;
with Ada.Text_IO;
with Ada.Unchecked_Conversion;
with Interfaces;
with System;

procedure Values is
   use Interfaces;

   type Point is record
      X, Y : Integer_32;
   end record;

   type Segment is record
      From, To : Point;
      Tag      : Unsigned_8;
   end record;

   type Color is (Red, Green, Blue);

   type Triple is array (1 .. 3) of Integer_32;
   type Centered is array (-1 .. 1) of Integer_32;
   type Grid is array (1 .. 2, 1 .. 3) of Integer_32;
   type Vector is array (Integer range <>) of Integer_32;

   Sink : System.Address with Volatile;

   Tab : constant Character := Ada.Characters.Latin_1.HT;


   procedure Reached (Checkpoint : String) with No_Inline;
   procedure Reached (Checkpoint : String) is
   begin
      Sink := Checkpoint'Address;
   end Reached;

   --  Keeps a value alive, and where the program put it, past the
   --  checkpoint.
   procedure Keep (Value : System.Address) with No_Inline;
   procedure Keep (Value : System.Address) is
   begin
      Sink := Value;
   end Keep;

   procedure Truth (Checkpoint, Path, Kind, Value : String) is
   begin
      Ada.Text_IO.Put_Line
        ("TRUTH" & Tab & Checkpoint & Tab & Path & Tab & Kind & Tab & Value);
      Ada.Text_IO.Flush;
   end Truth;

   function Decimal (Value : Long_Long_Integer) return String is
     (Ada.Strings.Fixed.Trim (Long_Long_Integer'Image (Value), Ada.Strings.Left));


   function Hexadecimal (Bits : Unsigned_64) return String is
      Digits_Of : constant String := "0123456789abcdef";
      Text      : String (1 .. 16);
      Rest      : Unsigned_64 := Bits;
      First     : Positive := Text'Last;
   begin
      for Index in reverse Text'Range loop
         Text (Index) := Digits_Of (Natural (Rest and 15) + 1);
         Rest := Shift_Right (Rest, 4);
         First := Index;
         exit when Rest = 0;
      end loop;
      return "0x" & Text (First .. Text'Last);
   end Hexadecimal;

   function F32_Bits is new Ada.Unchecked_Conversion (Float, Unsigned_32);
   function F64_Bits is new Ada.Unchecked_Conversion (Long_Float, Unsigned_64);

   function Add (A, B : Integer) return Integer with No_Inline;
   function Add (A, B : Integer) return Integer is
      Total : aliased Integer := A + B;
   begin
      Keep (Total'Address);
      return Total;
   end Add;




   function Returns_Int (N : Integer) return Integer with No_Inline;
   function Returns_Int (N : Integer) return Integer is
      Value : constant Integer := N * (-11);
   begin
      Truth ("returned-int", "returns_int", "int", Decimal (Long_Long_Integer (Value)));
      Reached ("returned-int");
      return Value;
   end Returns_Int;

   function Returns_Float (N : Integer) return Long_Float with No_Inline;
   function Returns_Float (N : Integer) return Long_Float is
      Value : constant Long_Float := -6.5 * Long_Float (N);
   begin
      Truth ("returned-float", "returns_float", "f64", Hexadecimal (F64_Bits (Value)));
      Reached ("returned-float");
      return Value;
   end Returns_Float;

   function Returns_Boolean (N : Integer) return Boolean with No_Inline;
   function Returns_Boolean (N : Integer) return Boolean is
      Value : constant Boolean := N > 0;
   begin
      Truth ("returned-boolean", "returns_boolean", "summary", (if Value then "true" else "false"));
      Reached ("returned-boolean");
      return Value;
   end Returns_Boolean;


   N : constant Integer := Ada.Command_Line.Argument_Count + 3;

   procedure Scalars with No_Inline;
   procedure Scalars is
      Small   : aliased Integer_8 := -5;
      Wide    : aliased Unsigned_16 := 65_000;
      Big     : aliased Long_Long_Integer := -(2 ** 40);
      Single  : aliased Float := 1.5;
      Precise : aliased Long_Float := -0.1;
      Flag    : aliased Boolean := True;
      Letter  : aliased Character := 'q';
   begin
      Truth ("scalars", "small", "int", Decimal (Long_Long_Integer (Small)));
      Truth ("scalars", "wide", "int", Decimal (Long_Long_Integer (Wide)));
      Truth ("scalars", "big", "int", Decimal (Big));
      Truth ("scalars", "single", "f32", Hexadecimal (Unsigned_64 (F32_Bits (Single))));
      Truth ("scalars", "precise", "f64", Hexadecimal (F64_Bits (Precise)));
      Truth ("scalars", "flag", "summary", "true");
      Truth ("scalars", "letter", "int", Decimal (Character'Pos (Letter)));
      Reached ("scalars");
      Keep (Small'Address); Keep (Wide'Address); Keep (Big'Address);
      Keep (Single'Address); Keep (Precise'Address); Keep (Flag'Address);
      Keep (Letter'Address);
   end Scalars;

   procedure Records with No_Inline;
   procedure Records is
      Origin  : aliased Point := (3, -4);
      Line    : aliased Segment := ((1, 2), (5, 6), 9);
      Numbers : aliased Triple := [10, 20, 30];
      Shifted : aliased Centered := [7, 8, 9];
      Table   : aliased Grid := [[11, 12, 13], [21, 22, 23]];
      Shade   : aliased Color := Green;
   begin
      Truth ("records", "origin.x", "int", Decimal (Long_Long_Integer (Origin.X)));
      Truth ("records", "origin.y", "int", Decimal (Long_Long_Integer (Origin.Y)));
      Truth ("records", "line.to.y", "int", Decimal (Long_Long_Integer (Line.To.Y)));
      Truth ("records", "line.tag", "int", Decimal (Long_Long_Integer (Line.Tag)));
      for Index in Numbers'Range loop
         Truth ("records", "numbers.(" & Decimal (Long_Long_Integer (Index)) & ")", "int",
                Decimal (Long_Long_Integer (Numbers (Index))));
      end loop;
      for Index in Shifted'Range loop
         Truth ("records", "shifted.(" & Decimal (Long_Long_Integer (Index)) & ")", "int",
                Decimal (Long_Long_Integer (Shifted (Index))));
      end loop;
      Truth ("records", "table.(2,1)", "int", Decimal (Long_Long_Integer (Table (2, 1))));
      Truth ("records", "table.(1,3)", "int", Decimal (Long_Long_Integer (Table (1, 3))));
      Truth ("records", "shade", "symbol", (if Shade = Red or Shade = Blue then "other" else "green"));
      Reached ("records");
      Keep (Origin'Address); Keep (Line'Address); Keep (Numbers'Address);
      Keep (Shifted'Address); Keep (Table'Address); Keep (Shade'Address);
   end Records;



   procedure Bounded (Text : String; Items : Vector) with No_Inline;
   procedure Bounded (Text : String; Items : Vector) is
      Copy : aliased Vector (Items'First .. Items'Last) := Items;
   begin
      Truth ("bounded", "text", "summary", """" & Text & """");
      for Index in Items'Range loop
         Truth ("bounded", "items.(" & Decimal (Long_Long_Integer (Index)) & ")", "int",
                Decimal (Long_Long_Integer (Items (Index))));
         Truth ("bounded", "copy.(" & Decimal (Long_Long_Integer (Index)) & ")", "int",
                Decimal (Long_Long_Integer (Copy (Index))));
      end loop;
      Reached ("bounded");
      Keep (Copy'Address);
   end Bounded;

   procedure Strings with No_Inline;
   procedure Strings is
      use Ada.Strings.Unbounded;
      Word    : aliased String (1 .. 5) := "hello";
      Grown   : aliased Unbounded_String := To_Unbounded_String ("unbounded");
      Nothing : aliased Unbounded_String;
   begin
      Append (Grown, " text");
      Truth ("strings", "word", "summary", """hello""");
      Truth ("strings", "grown", "summary", """" & To_String (Grown) & """");
      Truth ("strings", "nothing", "summary", """""");
      Reached ("strings");
      Keep (Word'Address); Keep (Grown'Address); Keep (Nothing'Address);
   end Strings;

begin
   Scalars;
   Records;
   declare
      Word : constant String := "shelling";
   begin
      Bounded (Word (2 .. 4), [-2 => 5, -1 => 6, 0 => 7]);
   end;
   Strings;
   if Add (2, 3) /= 5 then
      raise Program_Error with "add";
   end if;
   if Returns_Int (N) /= N * (-11) then
      raise Program_Error with "returns_int";
   end if;
   if Returns_Float (N) /= -6.5 * Long_Float (N) then
      raise Program_Error with "returns_float";
   end if;
   if not Returns_Boolean (N) then
      raise Program_Error with "returns_boolean";
   end if;
end Values;
