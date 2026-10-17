"""SQLAlchemy 2 (ORM, over psycopg) against Chronos: create_all, a session's inserts, queries, a relationship."""
import os

from sqlalchemy import ForeignKey, String, create_engine, func, select
from sqlalchemy.exc import IntegrityError
from sqlalchemy.orm import DeclarativeBase, Mapped, Session, mapped_column, relationship

URL = os.environ.get("CHRONOS_URL", "postgres://tester@127.0.0.1:5433/main")
engine = create_engine(URL.replace("postgres://", "postgresql+psycopg://", 1))


class Base(DeclarativeBase):
    pass


class Author(Base):
    __tablename__ = "drv_sa_author"
    id: Mapped[int] = mapped_column(primary_key=True)
    name: Mapped[str] = mapped_column(String(50), unique=True)
    books: Mapped[list["Book"]] = relationship(back_populates="author")


class Book(Base):
    __tablename__ = "drv_sa_book"
    id: Mapped[int] = mapped_column(primary_key=True)
    title: Mapped[str]
    pages: Mapped[int | None]
    author_id: Mapped[int] = mapped_column(ForeignKey("drv_sa_author.id"))
    author: Mapped[Author] = relationship(back_populates="books")


Base.metadata.drop_all(engine)
Base.metadata.create_all(engine)
with Session(engine) as s:
    ada = Author(name="Ada", books=[Book(title="Notes", pages=120), Book(title="Letters", pages=None)])
    s.add_all([ada, Author(name="Bob")])
    s.commit()
    assert ada.id is not None, "the serial key came back"
    assert s.scalars(select(Author.name).order_by(Author.name)).all() == ["Ada", "Bob"]
    counts = s.execute(
        select(Author.name, func.count(Book.id)).join(Book, isouter=True).group_by(Author.name).order_by(Author.name)
    ).all()
    assert counts == [("Ada", 2), ("Bob", 0)], counts
    book = s.scalars(select(Book).where(Book.title == "Notes")).one()
    book.pages = 121
    s.commit()
    assert s.scalar(select(Book.pages).where(Book.id == book.id)) == 121
    s.add(Author(name="Ada"))
    try:
        s.commit()
        raise SystemExit("a duplicate name was stored")
    except IntegrityError:
        s.rollback()
    s.delete(book)
    s.commit()
    assert s.scalar(select(func.count()).select_from(Book)) == 1
Base.metadata.drop_all(engine)
print("ok")
