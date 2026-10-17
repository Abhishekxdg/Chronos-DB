//! Monte Carlo Tree Search over worlds: every node of the search tree is a world holding a
//! Connect Four position in a table. Expanding a node forks its world and plays the move with
//! SQL; a rollout forks a throwaway world, fills the board with random moves, lets SQL find who
//! completed four in a row first, and discards it. Visit and win counts are backed up the tree
//! (UCT). After each real move, the worlds of subtrees that can no longer be reached are dropped,
//! leaves first. MCTS plays GAMES games against a random player, alternating who starts.
//!
//!   cargo run --release --example mcts
//!   env: GAMES (default 20), ITERS (MCTS iterations per move, default 200), SEED
use chronos::sql::Value;
use chronos::{Db, MAIN};
use std::time::{Duration, Instant};

#[path = "stats/mod.rs"]
mod stats;
use stats::{Rng, cpu_s, load, mem_mb};

fn env(name: &str, default: usize) -> usize {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

const COLS: usize = 7;
const ROWS: usize = 6;

// A cell's id is col * 8 + row: a line of four steps by 1 (up), 8 (across), 9 or 7 (diagonals),
// and the unused rows 6 and 7 keep a line from wrapping into the next column.
/// Who completed four in a row first (by move number), or 0.
const WINNER: &str = "select a.who, greatest(a.mv, b.mv, c.mv, d.mv) as done
    from cells a, unnest(array[1, 7, 8, 9]) k
    join cells b on b.id = a.id + k join cells c on c.id = a.id + 2 * k join cells d on d.id = a.id + 3 * k
    where b.who = a.who and c.who = a.who and d.who = a.who
    order by done limit 1";

struct Node {
    world: String,
    parent: Option<usize>,
    children: Vec<usize>,
    untried: Vec<usize>, // legal columns not expanded yet
    mv: usize,           // the column played to reach this node
    mover: i64,          // the player who played it (1 or 2); 0 at the root
    heights: [usize; COLS],
    board: [i64; 56],    // who holds each cell id, for the rollouts' self-check
    moves: i64,          // moves on the board
    result: Option<i64>, // Some(winner, or 0 for a draw) once the game is over here
    visits: f64,
    wins: f64, // for `mover`
    alive: bool,
}

struct Tree<'a> {
    db: &'a Db,
    nodes: Vec<Node>,
    game: usize,
    created: usize,
    discarded: usize,
    alive: usize,
    peak: usize,
    rollouts: usize,
}

fn sql(db: &Db, w: &str, q: &str) -> Vec<Vec<Value>> {
    chronos::sql::run(db, w, q, &[]).unwrap().pop().unwrap().rows
}

/// Whether the piece at `id` is part of four in a row (the check SQL's answer is compared with).
fn four(board: &[i64; 56], id: usize) -> bool {
    let who = board[id];
    let at = |i: i64| if (0..56).contains(&i) && (i % 8) < ROWS as i64 { board[i as usize] } else { 0 };
    [1i64, 7, 8, 9].iter().any(|&k| {
        let run = |dir: i64| (1..4).take_while(|&s| at(id as i64 + dir * s * k) == who).count();
        run(1) + run(-1) + 1 >= 4
    })
}

fn winner(db: &Db, w: &str) -> i64 {
    match sql(db, w, WINNER).first().map(|r| &r[0]) {
        Some(Value::Int(p)) => *p,
        _ => 0,
    }
}

impl Tree<'_> {
    fn fork(&mut self, from: &str, name: &str) {
        self.db.fork(from, name).unwrap();
        self.created += 1;
        self.alive += 1;
        self.peak = self.peak.max(self.alive);
    }

    fn discard(&mut self, name: &str) {
        self.db.discard(name).unwrap();
        self.discarded += 1;
        self.alive -= 1;
    }

    /// A child of `p`: its world forked from p's, with column `col` played in SQL.
    fn expand(&mut self, p: usize, col: usize) -> usize {
        let id = self.nodes.len();
        let world = format!("g{}n{id}", self.game);
        let parent_world = self.nodes[p].world.clone();
        self.fork(&parent_world, &world);
        let (moves, who) = (self.nodes[p].moves, if self.nodes[p].moves % 2 == 0 { 1 } else { 2 });
        sql(
            self.db,
            &world,
            &format!(
                "insert into cells select {col} * 8 + count(*), {col}, count(*), {who}, {} from cells where col = {col}",
                moves + 1
            ),
        );
        let (mut heights, mut board) = (self.nodes[p].heights, self.nodes[p].board);
        board[col * 8 + heights[col]] = who;
        heights[col] += 1;
        let result = match winner(self.db, &world) {
            0 if moves + 1 == (COLS * ROWS) as i64 => Some(0),
            0 => None,
            w => Some(w),
        };
        let untried = if result.is_some() { vec![] } else { (0..COLS).filter(|&c| heights[c] < ROWS).collect() };
        self.nodes.push(Node {
            world,
            parent: Some(p),
            children: vec![],
            untried,
            mv: col,
            mover: who,
            heights,
            board,
            moves: moves + 1,
            result,
            visits: 0.0,
            wins: 0.0,
            alive: true,
        });
        self.nodes[p].children.push(id);
        self.nodes[p].untried.retain(|&c| c != col);
        id
    }

    /// A random game from node `n` to a full board, played in a throwaway fork; SQL says who won.
    fn rollout(&mut self, n: usize, r: &mut Rng) -> i64 {
        if let Some(w) = self.nodes[n].result {
            return w;
        }
        let world = format!("g{}r{}", self.game, self.rollouts);
        self.rollouts += 1;
        let from = self.nodes[n].world.clone();
        self.fork(&from, &world);
        let (mut h, mut moves, mut board) = (self.nodes[n].heights, self.nodes[n].moves, self.nodes[n].board);
        let (mut rows, mut first) = (vec![], 0);
        while moves < (COLS * ROWS) as i64 {
            let open: Vec<usize> = (0..COLS).filter(|&c| h[c] < ROWS).collect();
            let c = open[r.below(open.len())];
            moves += 1;
            let (id, who) = (c * 8 + h[c], 2 - moves % 2);
            rows.push(format!("({id}, {c}, {}, {who}, {moves})", h[c]));
            board[id] = who;
            if first == 0 && four(&board, id) {
                first = who;
            }
            h[c] += 1;
        }
        sql(self.db, &world, &format!("insert into cells values {}", rows.join(",")));
        let w = winner(self.db, &world);
        assert_eq!(w, first, "SQL and the check disagree on who won");
        self.discard(&world);
        w
    }

    fn uct(&self, p: usize) -> usize {
        let ln = self.nodes[p].visits.ln();
        let score = |c: usize| {
            let n = &self.nodes[c];
            n.wins / n.visits + 1.4 * (ln / n.visits).sqrt()
        };
        *self.nodes[p].children.iter().max_by(|&&a, &&b| score(a).total_cmp(&score(b))).unwrap()
    }

    fn search(&mut self, root: usize, iters: usize, r: &mut Rng) -> usize {
        for _ in 0..iters {
            let mut n = root;
            while self.nodes[n].untried.is_empty() && !self.nodes[n].children.is_empty() {
                n = self.uct(n);
            }
            if !self.nodes[n].untried.is_empty() {
                let col = self.nodes[n].untried[r.below(self.nodes[n].untried.len())];
                n = self.expand(n, col);
            }
            let w = self.rollout(n, r);
            let mut up = Some(n);
            while let Some(i) = up {
                let node = &mut self.nodes[i];
                node.visits += 1.0;
                node.wins += if w == 0 {
                    0.5
                } else if w == node.mover {
                    1.0
                } else {
                    0.0
                };
                up = node.parent;
            }
        }
        let most = self.nodes[root].children.iter().max_by(|&&a, &&b| {
            self.nodes[a].visits.total_cmp(&self.nodes[b].visits).then(b.cmp(&a)) // ties: first expanded
        });
        self.nodes[*most.unwrap()].mv
    }

    /// Drops the worlds under `n`, leaves first (a world with forks can't be dropped).
    fn drop_subtree(&mut self, n: usize) {
        for c in self.nodes[n].children.clone() {
            self.drop_subtree(c);
        }
        if self.nodes[n].alive {
            let w = self.nodes[n].world.clone();
            self.discard(&w);
            self.nodes[n].alive = false;
        }
    }

    /// Plays `col` for real from `root`: the new root is that child, and every other subtree goes.
    fn play(&mut self, root: usize, col: usize) -> usize {
        let next = match self.nodes[root].children.iter().find(|&&c| self.nodes[c].mv == col) {
            Some(&c) => c,
            None => self.expand(root, col),
        };
        for c in self.nodes[root].children.clone() {
            if c != next {
                self.drop_subtree(c);
            }
        }
        self.nodes[root].children = vec![next];
        self.nodes[root].untried.clear();
        next
    }
}

fn main() {
    let (games, iters, seed) = (env("GAMES", 20), env("ITERS", 200), env("SEED", 7) as u64);
    let db = Db::new();
    sql(&db, MAIN, "create table cells (id int primary key, col int, row int, who int, mv int)");
    println!("{games} games, MCTS ({iters} iterations per move) against a random player, load {:.2}", load());
    let (mut won, mut lost, mut drawn) = (0, 0, 0);
    let (mut created, mut discarded, mut peak, mut total_iters, mut search_time) = (0, 0, 0, 0, Duration::ZERO);
    let (c0, t0) = (cpu_s(), Instant::now());
    let mut peak_mem = 0f64;
    for g in 0..games {
        let mut r = Rng(seed ^ (g as u64 + 1).wrapping_mul(0x9E37_79B9));
        let mcts_player = if g % 2 == 0 { 1 } else { 2 }; // alternate who starts
        let mut t = Tree { db: &db, nodes: vec![], game: g, created: 0, discarded: 0, alive: 0, peak: 0, rollouts: 0 };
        let root_world = format!("g{g}");
        t.fork(MAIN, &root_world);
        t.nodes.push(Node {
            world: root_world,
            parent: None,
            children: vec![],
            untried: (0..COLS).collect(),
            mv: 0,
            mover: 0,
            heights: [0; COLS],
            board: [0; 56],
            moves: 0,
            result: None,
            visits: 0.0,
            wins: 0.0,
            alive: true,
        });
        let mut root = 0;
        while t.nodes[root].result.is_none() {
            let to_move = if t.nodes[root].moves % 2 == 0 { 1 } else { 2 };
            let col = if to_move == mcts_player {
                let s = Instant::now();
                let c = t.search(root, iters, &mut r);
                search_time += s.elapsed();
                total_iters += iters;
                c
            } else {
                let open: Vec<usize> = (0..COLS).filter(|&c| t.nodes[root].heights[c] < ROWS).collect();
                open[r.below(open.len())]
            };
            root = t.play(root, col);
        }
        let result = t.nodes[root].result.unwrap();
        // the board as the game ended, straight from the final world
        let board = sql(&db, &t.nodes[root].world, "select count(*) from cells");
        match result {
            0 => drawn += 1,
            w if w == mcts_player => won += 1,
            _ => lost += 1,
        }
        peak_mem = peak_mem.max(mem_mb());
        let moves = t.nodes[root].moves;
        println!(
            "game {g}: MCTS as player {mcts_player}, {} after {moves} moves ({:?} cells); {} worlds created, {} dropped during play, peak {} alive",
            match result {
                0 => "draw".to_string(),
                w if w == mcts_player => "MCTS won".into(),
                _ => "random won".into(),
            },
            board[0][0],
            t.created,
            t.discarded,
            t.peak
        );
        // the game is over: drop what's left (the real game's chain of worlds), leaves first
        t.drop_subtree(0);
        assert_eq!(t.alive, 0);
        created += t.created;
        discarded += t.discarded;
        peak = peak.max(t.peak);
    }
    assert_eq!(db.branches().len(), 1);
    let wall = t0.elapsed().as_secs_f64();
    println!(
        "\n| games | iterations per move | MCTS won | lost | drawn | worlds created | discarded | peak alive | ms per iteration | total s | CPU s | peak mem MB | load |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    println!(
        "| {games} | {iters} | {won} | {lost} | {drawn} | {created} | {discarded} | {peak} | {:.3} | {wall:.1} | {:.1} | {peak_mem:.0} | {:.2} |",
        search_time.as_secs_f64() * 1e3 / total_iters as f64,
        cpu_s() - c0,
        load()
    );
}
