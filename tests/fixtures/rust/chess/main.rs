




use std::hint::black_box;

microscope_views::microscope_views_file!("tests/fixtures/rust/chess/chess.views");
microscope_views::microscope_visualizer!("chess-board", "tests/fixtures/rust/chess/chess-board.js");

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Color {
    White,
    Black,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Pawn,
    Knight,
    Bishop,
    Rook,
    Queen,
    King,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Piece {
    pub color: Color,
    pub kind: Kind,
}



#[derive(Clone, Copy, Debug)]
pub struct CastlingRights(pub u8);


#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Square(pub u8);

#[derive(Clone, Copy, Debug)]
pub struct Move {
    pub from: Square,
    pub to: Square,
}

pub struct Board {

    pub pieces: [u64; 6],

    pub colors: [u64; 2],
    pub mailbox: [Option<Piece>; 64],
    pub side_to_move: Color,
    pub castling: CastlingRights,
    pub en_passant: Option<Square>,
    pub fullmove: u32,
}

const BACK_RANK: [Kind; 8] = [
    Kind::Rook,
    Kind::Knight,
    Kind::Bishop,
    Kind::Queen,
    Kind::King,
    Kind::Bishop,
    Kind::Knight,
    Kind::Rook,
];

impl Board {
    pub fn start() -> Self {
        let mut board = Self {
            pieces: [0; 6],
            colors: [0; 2],
            mailbox: [None; 64],
            side_to_move: Color::White,
            castling: CastlingRights(0b1111),
            en_passant: None,
            fullmove: 1,
        };
        for file in 0..8 {
            board.put(Square(file), Piece { color: Color::White, kind: BACK_RANK[usize::from(file)] });
            board.put(Square(8 + file), Piece { color: Color::White, kind: Kind::Pawn });
            board.put(Square(48 + file), Piece { color: Color::Black, kind: Kind::Pawn });
            board.put(Square(56 + file), Piece { color: Color::Black, kind: BACK_RANK[usize::from(file)] });
        }
        board
    }

    fn put(&mut self, square: Square, piece: Piece) {
        let bit = 1_u64 << square.0;
        self.pieces[piece.kind as usize] |= bit;
        self.colors[piece.color as usize] |= bit;
        self.mailbox[usize::from(square.0)] = Some(piece);
    }

    fn take(&mut self, square: Square) -> Option<Piece> {
        let piece = self.mailbox[usize::from(square.0)].take()?;
        let bit = 1_u64 << square.0;
        self.pieces[piece.kind as usize] &= !bit;
        self.colors[piece.color as usize] &= !bit;
        Some(piece)
    }


    pub fn make_move(&mut self, mv: Move) -> Option<Piece> {
        let piece = self.take(mv.from).expect("a piece moves");
        let captured = self.take(mv.to);
        self.put(mv.to, piece);
        self.en_passant = (piece.kind == Kind::Pawn && mv.to.0.abs_diff(mv.from.0) == 16)
            .then(|| Square((mv.from.0 + mv.to.0) / 2));
        if piece.kind == Kind::King {
            self.castling.0 &= if piece.color == Color::White { 0b1100 } else { 0b0011 };

            if mv.to.0.abs_diff(mv.from.0) == 2 {
                let (from, to) = if mv.to.0 > mv.from.0 { (mv.to.0 + 1, mv.to.0 - 1) } else { (mv.to.0 - 2, mv.to.0 + 1) };
                let rook = self.take(Square(from)).expect("a rook castles");
                self.put(Square(to), rook);
            }
        }
        if self.side_to_move == Color::Black {
            self.fullmove += 1;
        }
        self.side_to_move = match self.side_to_move {
            Color::White => Color::Black,
            Color::Black => Color::White,
        };
        captured
    }
}


fn square(name: &str) -> Square {
    let bytes = name.as_bytes();
    Square((bytes[1] - b'1') * 8 + (bytes[0] - b'a'))
}

fn main() {
    let mut board = Board::start();
    let game = ["e2e4", "e7e5", "g1f3", "b8c6", "f1b5", "a7a6", "b5c6", "d7c6", "e1g1"];
    for text in game {
        let mv = Move { from: square(&text[..2]), to: square(&text[2..]) };
        let captured = board.make_move(black_box(mv));
        println!("{text}{}", if captured.is_some() { " takes" } else { "" });
    }
    black_box(&board);
}
