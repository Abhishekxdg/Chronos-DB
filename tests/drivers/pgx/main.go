// pgx v5 against Chronos: typed parameters (binary), a batch, a transaction, CopyFrom, an error's SQLSTATE.
package main

import (
	"context"
	"errors"
	"fmt"
	"os"
	"time"

	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgconn"
)

func must(err error) {
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func check(ok bool, what string) {
	if !ok {
		fmt.Fprintln(os.Stderr, "wrong:", what)
		os.Exit(1)
	}
}

func main() {
	url := os.Getenv("CHRONOS_URL")
	if url == "" {
		url = "postgres://tester@127.0.0.1:5433/main"
	}
	ctx := context.Background()
	conn, err := pgx.Connect(ctx, url)
	must(err)
	defer conn.Close(ctx)

	_, err = conn.Exec(ctx, "drop table if exists drv_pgx")
	must(err)
	_, err = conn.Exec(ctx, "create table drv_pgx (id bigint primary key, name text not null unique, score double precision, ok boolean, at timestamptz, tags text[])")
	must(err)
	at := time.Date(2026, 1, 2, 3, 4, 5, 0, time.UTC)
	_, err = conn.Exec(ctx, "insert into drv_pgx values ($1, $2, $3, $4, $5, $6)", int64(1), "Ada", 9.5, true, at, []string{"x", "y"})
	must(err)
	_, err = conn.Exec(ctx, "insert into drv_pgx (id, name) values ($1, $2)", int64(2), "Bob")
	must(err)

	var (
		id    int64
		name  string
		score *float64
		ok    *bool
		got   *time.Time
		tags  []string
	)
	must(conn.QueryRow(ctx, "select * from drv_pgx where id = $1", int64(1)).Scan(&id, &name, &score, &ok, &got, &tags))
	check(id == 1 && name == "Ada" && *score == 9.5 && *ok && got.Equal(at) && len(tags) == 2 && tags[1] == "y", "the first row")
	must(conn.QueryRow(ctx, "select * from drv_pgx where id = $1", int64(2)).Scan(&id, &name, &score, &ok, &got, &tags))
	check(score == nil && ok == nil && got == nil && tags == nil, "the nulls of the second row")

	// several statements in one round trip
	batch := &pgx.Batch{}
	for i := 10; i < 15; i++ {
		batch.Queue("insert into drv_pgx (id, name) values ($1, $2)", int64(i), fmt.Sprintf("n%d", i))
	}
	must(conn.SendBatch(ctx, batch).Close())

	// rows copied in
	n, err := conn.CopyFrom(ctx, pgx.Identifier{"drv_pgx"}, []string{"id", "name"}, pgx.CopyFromRows([][]any{{int64(20), "c1"}, {int64(21), "c2"}}))
	must(err)
	check(n == 2, "two rows copied")

	tx, err := conn.Begin(ctx)
	must(err)
	_, err = tx.Exec(ctx, "insert into drv_pgx (id, name) values (100, 'gone')")
	must(err)
	must(tx.Rollback(ctx))
	var count int64
	must(conn.QueryRow(ctx, "select count(*) from drv_pgx").Scan(&count))
	check(count == 9, "nine rows after the rollback")

	_, err = conn.Exec(ctx, "insert into drv_pgx (id, name) values (3, 'Ada')")
	var pgErr *pgconn.PgError
	check(errors.As(err, &pgErr) && pgErr.Code == "23505", "a duplicate name is 23505")

	_, err = conn.Exec(ctx, "drop table drv_pgx")
	must(err)
	fmt.Println("ok")
}
