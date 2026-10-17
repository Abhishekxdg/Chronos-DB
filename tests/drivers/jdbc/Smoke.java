// JDBC (pgjdbc) against Chronos: a PreparedStatement's typed parameters, a batch, a transaction, generated keys,
// an error's SQLSTATE.
import java.math.BigDecimal;
import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.PreparedStatement;
import java.sql.ResultSet;
import java.sql.SQLException;
import java.sql.Statement;
import java.sql.Timestamp;
import java.time.Instant;

public class Smoke {
    static void check(boolean ok, String what) {
        if (!ok) {
            throw new AssertionError(what);
        }
    }

    public static void main(String[] args) throws Exception {
        String url = System.getenv().getOrDefault("CHRONOS_JDBC_URL", "jdbc:postgresql://127.0.0.1:5433/main?user=tester");
        try (Connection c = DriverManager.getConnection(url)) {
            try (Statement s = c.createStatement()) {
                s.execute("drop table if exists drv_jdbc");
                s.execute("create table drv_jdbc (id serial primary key, name text not null unique, score numeric(10,2), ok boolean, at timestamptz)");
            }
            Timestamp at = Timestamp.from(Instant.parse("2026-01-02T03:04:05Z"));
            try (PreparedStatement p = c.prepareStatement(
                    "insert into drv_jdbc (name, score, ok, at) values (?, ?, ?, ?)", Statement.RETURN_GENERATED_KEYS)) {
                p.setString(1, "Ada");
                p.setBigDecimal(2, new BigDecimal("9.50"));
                p.setBoolean(3, true);
                p.setTimestamp(4, at);
                check(p.executeUpdate() == 1, "one row inserted");
                try (ResultSet k = p.getGeneratedKeys()) {
                    check(k.next() && k.getInt(1) == 1, "the generated key");
                }
            }
            // a statement run enough times for the driver to prepare it on the server
            try (PreparedStatement p = c.prepareStatement("select name, score, ok, at from drv_jdbc where id = ?")) {
                for (int i = 0; i < 8; i++) {
                    p.setInt(1, 1);
                    try (ResultSet r = p.executeQuery()) {
                        check(r.next(), "a row");
                        check(r.getString(1).equals("Ada") && r.getBigDecimal(2).equals(new BigDecimal("9.50")), "name and score");
                        check(r.getBoolean(3) && r.getTimestamp(4).equals(at), "boolean and timestamp");
                    }
                }
            }
            try (PreparedStatement p = c.prepareStatement("insert into drv_jdbc (name) values (?)")) {
                for (String n : new String[] {"b1", "b2", "b3"}) {
                    p.setString(1, n);
                    p.addBatch();
                }
                check(p.executeBatch().length == 3, "three in the batch");
            }
            c.setAutoCommit(false);
            try (Statement s = c.createStatement()) {
                s.executeUpdate("insert into drv_jdbc (name) values ('gone')");
            }
            c.rollback();
            c.setAutoCommit(true);
            try (Statement s = c.createStatement(); ResultSet r = s.executeQuery("select count(*), count(score) from drv_jdbc")) {
                check(r.next() && r.getLong(1) == 4 && r.getLong(2) == 1, "four rows, one score");
            }
            try (Statement s = c.createStatement()) {
                s.executeUpdate("insert into drv_jdbc (name) values ('Ada')");
                throw new AssertionError("a duplicate name was stored");
            } catch (SQLException e) {
                check("23505".equals(e.getSQLState()), "a duplicate name is 23505, not " + e.getSQLState());
            }
            try (ResultSet t = c.getMetaData().getTables(null, "public", "drv_jdbc", null)) {
                check(t.next(), "the table is in the metadata");
            }
            try (Statement s = c.createStatement()) {
                s.execute("drop table drv_jdbc");
            }
        }
        System.out.println("ok");
    }
}
