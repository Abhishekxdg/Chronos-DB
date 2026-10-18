"""Alembic (over SQLAlchemy and psycopg) against Chronos: three migrations up, all the way down and up again, each in
the transaction Alembic runs it in, then what SQLAlchemy's inspector reflects from the catalogs (what autogenerate and
schema browsers read)."""
import os
import pathlib
import tempfile

from alembic import command
from alembic.config import Config
from sqlalchemy import create_engine, inspect, text

URL = os.environ.get("CHRONOS_URL", "postgres://tester@127.0.0.1:5433/main").replace(
    "postgres://", "postgresql+psycopg://", 1
)
HEAD = '''revision = "{rev}"
down_revision = {down}
branch_labels = None
depends_on = None

import sqlalchemy as sa
from alembic import op
'''
MIGRATIONS = {
    "r1": (None, '''
def upgrade():
    op.create_table(
        "drv_alembic_account",
        sa.Column("id", sa.Integer, primary_key=True),
        sa.Column("email", sa.String(255), nullable=False, unique=True),
        sa.Column("created_at", sa.DateTime(timezone=True), server_default=sa.func.now(), nullable=False),
    )
    op.create_index("drv_alembic_account_created", "drv_alembic_account", ["created_at"])


def downgrade():
    op.drop_index("drv_alembic_account_created", table_name="drv_alembic_account")
    op.drop_table("drv_alembic_account")
'''),
    "r2": ('"r1"', '''
def upgrade():
    op.add_column("drv_alembic_account", sa.Column("balance", sa.Numeric(12, 2), server_default="0", nullable=False))
    op.create_table(
        "drv_alembic_transfer",
        sa.Column("id", sa.Integer, primary_key=True),
        sa.Column("account_id", sa.Integer, sa.ForeignKey("drv_alembic_account.id", ondelete="CASCADE"), nullable=False),
        sa.Column("amount", sa.Numeric(12, 2), nullable=False),
        sa.CheckConstraint("amount <> 0", name="drv_alembic_transfer_nonzero"),
    )
    account = sa.table("drv_alembic_account", sa.column("id", sa.Integer), sa.column("email", sa.String))
    op.bulk_insert(account, [{"id": 1, "email": "ada@example.com"}, {"id": 2, "email": "bo@example.com"}])


def downgrade():
    op.drop_table("drv_alembic_transfer")
    op.drop_column("drv_alembic_account", "balance")
'''),
    "r3": ('"r2"', '''
def upgrade():
    op.alter_column("drv_alembic_account", "email", type_=sa.Text, existing_nullable=False)
    op.alter_column("drv_alembic_account", "email", new_column_name="address")
    op.execute("update drv_alembic_account set balance = 10 where id = 1")


def downgrade():
    op.alter_column("drv_alembic_account", "address", new_column_name="email")
    op.alter_column("drv_alembic_account", "email", type_=sa.String(255), existing_nullable=False)
'''),
}

root = pathlib.Path(tempfile.mkdtemp())
cfg = Config(str(root / "alembic.ini"))
command.init(cfg, str(root / "mig"))
cfg = Config(str(root / "alembic.ini"))
cfg.set_main_option("script_location", str(root / "mig"))
cfg.set_main_option("sqlalchemy.url", URL.replace("%", "%%"))
for rev, (down, body) in MIGRATIONS.items():
    (root / "mig" / "versions" / f"{rev}.py").write_text(HEAD.format(rev=rev, down=down) + body)

engine = create_engine(URL)


def version():
    with engine.connect() as c:
        return c.execute(text("select version_num from alembic_version")).scalar()


def tables():
    return {t for t in inspect(engine).get_table_names() if t.startswith("drv_alembic")}


with engine.begin() as c:  # a rerun starts from nothing
    c.execute(text("drop table if exists drv_alembic_transfer, drv_alembic_account, alembic_version"))

command.upgrade(cfg, "head")
assert version() == "r3", version()
assert tables() == {"drv_alembic_account", "drv_alembic_transfer"}, tables()
with engine.connect() as c:
    rows = c.execute(text("select id, address, balance from drv_alembic_account order by id")).all()
assert [(r[0], r[1], str(r[2])) for r in rows] == [(1, "ada@example.com", "10.00"), (2, "bo@example.com", "0.00")], rows

# what the inspector reflects: columns, keys, indexes, checks
ins = inspect(engine)
cols = {c["name"]: c for c in ins.get_columns("drv_alembic_account")}
assert set(cols) == {"id", "address", "created_at", "balance"}, cols
assert str(cols["address"]["type"]) == "TEXT" and not cols["address"]["nullable"], cols["address"]
assert str(cols["balance"]["type"]) == "NUMERIC(12, 2)", cols["balance"]
assert ins.get_pk_constraint("drv_alembic_account")["constrained_columns"] == ["id"]
fks = ins.get_foreign_keys("drv_alembic_transfer")
assert len(fks) == 1 and fks[0]["referred_table"] == "drv_alembic_account", fks
assert fks[0]["referred_columns"] == ["id"] and fks[0]["options"].get("ondelete") == "CASCADE", fks
assert any(i["name"] == "drv_alembic_account_created" for i in ins.get_indexes("drv_alembic_account"))
assert any(u["column_names"] == ["address"] for u in ins.get_unique_constraints("drv_alembic_account")), (
    ins.get_unique_constraints("drv_alembic_account")
)
assert any(ch["name"] == "drv_alembic_transfer_nonzero" for ch in ins.get_check_constraints("drv_alembic_transfer"))

# down one, down to nothing, and up again
command.downgrade(cfg, "-1")
assert version() == "r2", version()
assert "email" in {c["name"] for c in inspect(engine).get_columns("drv_alembic_account")}
command.downgrade(cfg, "base")
assert version() is None and tables() == set(), (version(), tables())
command.upgrade(cfg, "head")
assert version() == "r3" and len(tables()) == 2
with engine.begin() as c:
    c.execute(text("insert into drv_alembic_transfer (account_id, amount) values (1, 5)"))
    c.execute(text("delete from drv_alembic_account where id = 1"))
    assert c.execute(text("select count(*) from drv_alembic_transfer")).scalar() == 0, "ON DELETE CASCADE"
print("alembic: ok")
