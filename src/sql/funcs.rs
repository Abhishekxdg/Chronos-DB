//! Scalar functions beyond the few the evaluator handles itself: text, math, NULL handling and
//! dates, with Postgres's names, argument rules and edge cases (checked against Postgres in
//! tests/pgdiff.rs).

use super::{Result, Type, Value, err, hstore, intarray, jsonpath, ltree, net, textext, time};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Scalar {
    /// a range type's constructor (by its oid), and what is done with ranges: see `range::call`
    RangeNew(u32),
    /// money's operators (0: money, 1: money over money, 2: a comparison) and functions (cashlarger, cashsmaller, cash_words)
    MoneyOp(u8),
    /// hashint4, hashtext, ... (see `hashfn`): the kind, and whether it is the extended form (with a seed)
    HashFn(u8, bool),
    /// geometric operators (args: a, b, the operator, the two types' ids) and functions (the arguments, then their types' ids)
    GeoOp,
    GeoOpF,
    GeoOpV,
    GeoFn(u8),
    /// a value's key for ORDER BY, where the type's order isn't its text's: money, time with time zone, ranges
    SortKey,
    /// time with time zone: comparisons 0..=5, + and - interval, as time, from time, AT TIME ZONE, extract (see `timetz`)
    TimeTzFn(u8),
    /// pg_stat_statements_reset()
    StatReset,
    /// a masked column's value, by its mode (see `mask`): made where a masked table is read, never by name
    Mask(u8),
    /// ARRAY[[1, 2], [3, 4]]: arrays of a dimension below, made one of more dimensions
    ArrayNest,
    TablespaceLocation,
    /// `a *< b` and the other record image comparisons (args: a, b, the operator, the two record types' places):
    /// see `super::record_image`
    RecordImage,
    /// intarray's functions and operators, by number (see `intarray`)
    IntArr(u8),
    /// hstore's and ltree's operators and functions, by number (see `hstore` and `ltree`)
    Hstore(u8),
    Ltree(u8),
    /// json's own functions (json_build_object, to_json, json_typeof, ...), by number: json text, as written or in
    /// Postgres's layouts, not jsonb (see `jsontext`)
    JsonFn(u8),
    /// xid8cmp, pg_snapshot_xmin/xmax, pg_visible_in_snapshot, and the txid_ ones (see `xid`)
    XidFn(u8),
    /// bit strings: & | # ~ << >> (0..5), x::bit(n), a bit string as an integer, get_bit and its kin
    BitOp(u8),
    BitFit,
    BitToInt,
    BitFn(u8),
    MoneyFn(u8),
    /// pg_lsn's arithmetic, as `lsn_call` numbers it
    LsnFn(u8),
    /// a cast WITHOUT FUNCTION between types of different representations: (value, from's oid, to's oid), its bits
    /// read as the other's (see `binary_cast`)
    BinaryCast,
    DateBin,
    /// x::timestamp(p), and the other time types' casts with a precision: (value, p), its fraction rounded to p digits
    TimePrec,
    /// date_add and date_subtract (true): a timestamptz moved by an interval on a zone's calendar
    DateAdd(bool),
    #[allow(dead_code)] // (made only by what binds point(x, y) through the geometric functions)
    PointNew,
    Normalize,
    IsNormalized,
    ParseIdent,
    TsLexize,
    /// booleq, boolne, boolgt, boollt, boolge, boolle: 0..5; booland_statefunc and boolor_statefunc: 6, 7
    BoolCmp(u8),
    /// float8_accum, float8_regr_accum, float8_combine, float8_regr_combine: 0..3 (see `stats::transition`)
    FloatAccum(u8),
    /// a real as numeric (float4_numeric): its six significant digits, `%.6g`
    Float4Numeric,
    /// a real as text (float4out): its float4 digits, not its double's
    Float4Text,
    /// a boolean[] cast to a text[] (or varchar[], bpchar[]): each element as boolean's cast to text writes it,
    /// `true` and `false`, not boolout's `t` and `f` an array of booleans is written with
    BoolArrayText,
    /// a real arithmetic's result held to 32 bits (float4pl and its kin round each result: `0.1::real + 0.2::real` is
    /// the real nearest 0.3), 22003 where a finite result passes a real's range
    Float4Round,
    /// what a real aggregate (sum) is fed: the value itself, marking the state to round its total to 32 bits as it
    /// adds
    Float4Acc,
    /// a real where it is written into a row, an array or JSON: its float4 digits (`9.8`, not `9.800000190734863`)
    /// as the number's text, still a real (the same 32 bits when read back as one)
    Float4Digits,
    MultiNew(u32),
    RangeOp,
    RangeSetOp,
    RangeBound(bool),
    RangeFlag(u8),
    RangeMerge,
    Concat,
    ConcatWs,
    Substr,
    /// substring(...) as SQL writes it (FROM/FOR/SIMILAR): substr, by the name its column takes
    Substring,
    Strpos,
    /// position(a IN b): strpos by its own name
    Position,
    Replace,
    Btrim,
    Ltrim,
    Rtrim,
    Left,
    Right,
    Lpad,
    Rpad,
    Repeat,
    Reverse,
    Translate,
    Unistr,
    /// text as "char", Postgres's one-byte type: the byte charin and text_char read (`\ooo` in octal, else the first
    /// byte), kept as the text charout writes for it (`\ooo` for a byte past ASCII, nothing for a zero byte)
    Char,
    Similarity,
    WordSimilarity,
    StrictWordSimilarity,
    ShowTrgm,
    TrgmTest,
    TrgmDist,
    Levenshtein,
    LevenshteinLessEqual,
    Soundex,
    Difference,
    Metaphone,
    Dmetaphone,
    DmetaphoneAlt,
    DaitchMokotoff,
    Unaccent,
    SplitPart,
    Initcap,
    OctetLength,
    Ascii,
    Chr,
    StartsWith,
    Ceil,
    /// ceiling(x): ceil by its other name (which its column takes)
    Ceiling,
    Floor,
    Trunc,
    Sqrt,
    Cbrt,
    /// erf(x) and erfc(x): the error function and its complement, the C library's (as Postgres's are)
    Erf,
    Erfc,
    Exp,
    Ln,
    Log,
    /// log10(x): log by its other name (which its column takes)
    Log10,
    Power,
    Mod,
    Sign,
    IsFinite,
    JustifyHours,
    JustifyDays,
    JustifyInterval,
    /// `(s1, e1) OVERLAPS (s2, e2)`: its four values, an end that is an interval being the length
    Overlaps,
    Degrees,
    Radians,
    Sin,
    Cos,
    Tan,
    Cot,
    Asin,
    Acos,
    Atan,
    Atan2,
    Sinh,
    Sind,
    Cosd,
    Tand,
    Cotd,
    Asind,
    Acosd,
    Atand,
    Atan2d,
    Asinh,
    Acosh,
    Atanh,
    Scale,
    MinScale,
    TrimScale,
    Cosh,
    Tanh,
    Div,
    /// numeric_inc(x): x + 1, numeric's own
    NumericInc,
    Gcd,
    Lcm,
    Factorial,
    WidthBucket,
    /// float4send (true) and float8send (false)
    FloatSend(bool),
    ToHex,
    Greatest,
    Least,
    Random,
    /// random_normal(mean, stddev): a number of that normal distribution (see `random_normal`)
    RandomNormal,
    /// uuid_generate_v1() (false) and uuid_generate_v1mc() (true): a time-based UUID, of a random node
    UuidV1(bool),
    SetSeed,
    Pi,
    Nullif,
    MakeDate,
    MakeTime,
    MakeTimestamp,
    MakeTimestampTz,
    MakeInterval,
    ToTimestamp,
    ToDate,
    JsonBuildObject,
    JsonBuildArray,
    /// JSON_OBJECT and JSON_ARRAY as SQL has them: a number of flags (1 an array, 2 ABSENT ON NULL, 4 WITH UNIQUE
    /// KEYS), then the keys and values, or the elements
    JsonCtor,
    /// a SQL/JSON value written `FORMAT JSON [ENCODING UTF8]`: (value, whether ENCODING was written, where the value
    /// and the FORMAT are in the statement's text), checked and read as json once its type is known (see `Binder::go`)
    JsonFormatted,
    /// x IS JSON [VALUE | OBJECT | ARRAY | SCALAR]: whether text reads as JSON of that kind
    IsJson,
    /// json_object(text[]) and json_object(keys text[], values text[]): an object of text values
    JsonObjectOf,
    ToJson,
    /// An array as a JSON array: `[array, element type id]`, the id put there by the binder
    ArrayJson,
    JsonTypeof,
    JsonArrayLength,
    JsonExtractPath,
    JsonExtractPathText,
    /// jsonb_path_exists, _match, _query_array, _query_first (which one) and whether it is the `_tz` form
    JsonPath(PathFn, bool),
    JsonSet,
    JsonSetLax,
    JsonInsert,
    JsonPretty,
    JsonDelete,
    JsonDeleteKeys,
    JsonDeletePath,
    JsonStripNulls,
    /// A call of a json function by a name Postgres names its column by, where the function here goes by another
    /// (`jsonb_set`, `to_jsonb`, `row_to_json`): its one argument, the call, as it is (the name is `SPELLED`'s)
    Spelled(u8),
    /// jsonb_contains, jsonb_contained, jsonb_exists, jsonb_exists_any, jsonb_exists_all, jsonb_concat and
    /// jsonb_delete (0 to 6): the functions behind jsonb's operators, made the operator when the call is read
    JsonOpFn(u8),
    /// pg_column_size(x): how many bytes x takes as a value (a varlena's four-byte header and its text)
    ColumnSize,
    Age,
    ToChar,
    Md5,
    ArrayOf, // ARRAY[...]
    /// a bigint converted to an oid, implicitly or by a cast: 22003 below 0 or past 32 bits (where an int4's negative
    /// value wraps around); no name calls it
    BigintOid,
    RegexpReplace,
    RegexpMatch,
    RegexpLike,
    SimilarEscape,
    /// A LIKE pattern written with another escape character, as one with the backslash: what
    /// `ESCAPE 'c'` reads it as (pattern, escape)
    LikeEscape,
    ToTsvector,
    JsonToTsvector,
    ToTsquery,
    PlaintoTsquery,
    PhrasetoTsquery,
    WebsearchToTsquery,
    TsRank,
    TsRankCd,
    XmlElement,
    XmlForest,
    XmlConcat,
    XmlPi,
    XmlRoot,
    XmlComment,
    XmlParse,
    XmlSerialize,
    XmlExistsFn,
    Xpath,
    XpathExists,
    /// xml_is_well_formed (0: as XML OPTION says), xml_is_well_formed_content (1) and xml_is_well_formed_document (2)
    XmlWellFormed(u8),
    IsDocument,
    TsHeadline,
    TsDelete,
    TsFilter,
    ArrayToTsvector,
    TsvectorToArray,
    NumNode,
    QueryTree,
    TsqueryPhrase,
    TsqueryAnd,
    TsqueryOr,
    TsqueryNot,
    TsqueryCmp,
    /// `q @> r`: tsq_mcontains
    TsqueryContains,
    TsRewrite,
    Setweight,
    Strip,
    TsConcat,
    StMakePoint,
    StPoint,
    StSetSrid,
    StSrid,
    StGeomFromText,
    StGeogFromText,
    StGeomFromGeoJson,
    StAsText,
    StAsEwkt,
    StAsGeoJson,
    StX,
    StY,
    StDistance,
    StDWithin,
    StDistanceSphere,
    GeoX,
    GeoY,
    ArrayAt,
    ArrayAppend,
    ArrayPrepend,
    ArrayCat,
    ArrayRemove,
    ArrayReplace,
    ArrayPosition,
    ArrayPositions,
    ArrayDims,
    ArrayNdims,
    ArrayFill,
    TrimArray,
    /// array_shuffle(a) and array_sample(a, n): the first dimension's items in a random order, all or n of them
    ArrayShuffle,
    ArraySample,
    /// GROUPING(a, b): rewritten to a constant for each grouping set (see `expand_grouping_sets`)
    Grouping,
    /// a[lo:hi]; an open bound is i64::MIN or i64::MAX
    ArraySlice,
    ArraySet,
    /// a[i][j]...: an element by a subscript for each dimension (see `mdarray::Arr::get`)
    ArrayAtN,
    /// a[i][j]... = v and a[lo:hi][lo:hi]... = v (see `assigned_at`): the array, the value, whether a slice, then
    /// the subscripts (two for each range of a slice)
    ArraySetN,
    SampleKeep,
    StringToArray,
    ArrayToString,
    NumNulls,
    NumNonnulls,
    EncodingToChar,
    /// pg_char_to_encoding: an encoding's number from its name, -1 for a name that is none
    CharToEncoding,
    Cardinality,
    ArrayLength,
    ArrayLower,
    ArrayUpper,
    RegexpCount,
    RegexpSubstr,
    RegexpInstr,
    RegexpSplit,
    Format,
    QuoteIdent,
    QuoteLiteral,
    QuoteNullable,
    // bytea's (and text's, where Postgres has them): see `bytes`
    Encode,
    Decode,
    Sha224,
    ToNumber,
    Sha256,
    Sha384,
    Sha512,
    GetByte,
    SetByte,
    BitLength,
    ConvertTo,
    ConvertFrom,
    Overlay,
    // the network address types' (see `net`)
    NetHost,
    NetAbbrev,
    NetBroadcast,
    NetNetmask,
    NetHostmask,
    NetNetwork,
    NetMasklen,
    NetFamily,
    NetSetMasklen,
    NetSameFamily,
    NetMerge,
    NetSet7bit,
    /// `inet(x)`, `cidr(x)`, `macaddr(x)` and `macaddr8(x)`: a call written as a type's name
    NetInet,
    NetCidr,
    NetMacaddr,
    NetMacaddr8,
    // pgcrypto's (see `pgcrypto`)
    Digest,
    Hmac,
    Crypt,
    GenSalt,
    GenRandomBytes,
    Encrypt,
    Decrypt,
    EncryptIv,
    DecryptIv,
    PgpSymEncrypt,
    PgpSymEncryptBytea,
    PgpSymDecrypt,
    PgpSymDecryptBytea,
    PgpKeyId,
    Armor,
    Dearmor,
}

/// The function a name calls, and how many arguments it takes.
pub(super) fn lookup(name: &str) -> Option<(Scalar, std::ops::RangeInclusive<usize>)> {
    use Scalar::*;
    const MANY: usize = usize::MAX;
    if let Some((k, arity)) = super::jsontext::lookup(name) {
        return Some((JsonFn(k), arity));
    }
    Some(match name {
        "concat" => (Concat, 1..=MANY),
        "concat_ws" => (ConcatWs, 2..=MANY),
        "substr" | "substring" => (Substr, 2..=3),
        "strpos" => (Strpos, 2..=2),
        "replace" => (Replace, 3..=3),
        "btrim" => (Btrim, 1..=2),
        "ltrim" => (Ltrim, 1..=2),
        "rtrim" => (Rtrim, 1..=2),
        "left" => (Left, 2..=2),
        "right" => (Right, 2..=2),
        "lpad" => (Lpad, 2..=3),
        "rpad" => (Rpad, 2..=3),
        "repeat" => (Repeat, 2..=2),
        "reverse" => (Reverse, 1..=1),
        // (what `||` of two texts calls, by its name: a user aggregate's `sfunc = textcat`)
        "textcat" => (Concat, 2..=2),
        // pg_stat_statements's: forget the statistics (see `stmtstats`) of a user, a database, a query id (0: any)
        "pg_stat_statements_reset" => (StatReset, 0..=3),
        // (there are no tablespaces but the default ones, which are in the data folder: '')
        "pg_tablespace_location" => (TablespaceLocation, 1..=1),
        // intarray's
        "icount" => (IntArr(intarray::ICOUNT), 1..=1),
        "sort" => (IntArr(intarray::SORT), 1..=2),
        "sort_asc" => (IntArr(intarray::SORT_ASC), 1..=1),
        "sort_desc" => (IntArr(intarray::SORT_DESC), 1..=1),
        "uniq" => (IntArr(intarray::UNIQ), 1..=1),
        "idx" => (IntArr(intarray::IDX), 2..=2),
        "subarray" => (IntArr(intarray::SUBARRAY), 2..=3),
        "intset" => (IntArr(intarray::INTSET), 1..=1),
        // hstore's functions (hstore(...) and delete(...) are chosen by their arguments' types: see `hstore_rewrite`)
        "hstore" => (Hstore(hstore::MAKE), 1..=2),
        "tconvert" => (Hstore(hstore::MAKE), 2..=2),
        "fetchval" => (Hstore(hstore::FETCH), 2..=2),
        "slice_array" => (Hstore(hstore::FETCH_MANY), 2..=2),
        "hs_concat" => (Hstore(hstore::CONCAT), 2..=2),
        "exist" | "isexists" => (Hstore(hstore::EXIST), 2..=2),
        "exists_any" => (Hstore(hstore::EXIST_ANY), 2..=2),
        "exists_all" => (Hstore(hstore::EXIST_ALL), 2..=2),
        "hs_contains" => (Hstore(hstore::CONTAINS), 2..=2),
        "hs_contained" => (Hstore(hstore::CONTAINED), 2..=2),
        "defined" | "isdefined" => (Hstore(hstore::DEFINED), 2..=2),
        "delete" => (Hstore(hstore::DELETE), 2..=2),
        "slice" => (Hstore(hstore::SLICE), 2..=2),
        "akeys" => (Hstore(hstore::AKEYS), 1..=1),
        "avals" => (Hstore(hstore::AVALS), 1..=1),
        "hstore_to_array" => (Hstore(hstore::TO_ARRAY), 1..=1),
        "hstore_to_matrix" => (Hstore(hstore::TO_MATRIX), 1..=1),
        "hstore_to_json" | "hstore_to_jsonb" => (Hstore(hstore::TO_JSON), 1..=1),
        "hstore_to_json_loose" | "hstore_to_jsonb_loose" => (Hstore(hstore::TO_JSON_LOOSE), 1..=1),
        // ltree's
        "nlevel" => (Ltree(ltree::NLEVEL), 1..=1),
        "subltree" => (Ltree(ltree::SUBLTREE), 3..=3),
        "subpath" => (Ltree(ltree::SUBPATH), 2..=3),
        "index" => (Ltree(ltree::INDEX), 2..=3),
        "text2ltree" => (Ltree(ltree::TEXT2LTREE), 1..=1),
        "ltree2text" => (Ltree(ltree::LTREE2TEXT), 1..=1),
        "lca" => (Ltree(ltree::LCA), 1..=8),
        "translate" => (Translate, 3..=3),
        "unistr" => (Unistr, 1..=1),
        "similarity" => (Similarity, 2..=2),
        "word_similarity" => (WordSimilarity, 2..=2),
        "strict_word_similarity" => (StrictWordSimilarity, 2..=2),
        "show_trgm" => (ShowTrgm, 1..=1),
        // the operators of pg_trgm, named for the binder to call: `%`, `<%`, `%>`, `<<%`, `%>>` and the
        // distances `<->`, `<<->`, `<->>`, `<<<->`, `<->>>` (see `trgm_op`)
        "__trgm_test" => (TrgmTest, 3..=3),
        "__trgm_dist" => (TrgmDist, 3..=3),
        "levenshtein" => (Levenshtein, 2..=5),
        "levenshtein_less_equal" => (LevenshteinLessEqual, 3..=6),
        "soundex" => (Soundex, 1..=1),
        "difference" => (Difference, 2..=2),
        "metaphone" => (Metaphone, 2..=2),
        "dmetaphone" => (Dmetaphone, 1..=1),
        "dmetaphone_alt" => (DmetaphoneAlt, 1..=1),
        "daitch_mokotoff" => (DaitchMokotoff, 1..=1),
        "unaccent" => (Unaccent, 1..=2),
        "split_part" => (SplitPart, 3..=3),
        "initcap" => (Initcap, 1..=1),
        "hashint2" => (HashFn(0, false), 1..=1),
        "hashint2extended" => (HashFn(0, true), 2..=2),
        "hashint4" => (HashFn(1, false), 1..=1),
        "hashint4extended" => (HashFn(1, true), 2..=2),
        "hashint8" => (HashFn(2, false), 1..=1),
        "hashint8extended" => (HashFn(2, true), 2..=2),
        "hashoid" => (HashFn(3, false), 1..=1),
        "hashoidextended" => (HashFn(3, true), 2..=2),
        "hashchar" => (HashFn(4, false), 1..=1),
        "hashcharextended" => (HashFn(4, true), 2..=2),
        "hashfloat4" => (HashFn(5, false), 1..=1),
        "hashfloat4extended" => (HashFn(5, true), 2..=2),
        "hashfloat8" => (HashFn(6, false), 1..=1),
        "hashfloat8extended" => (HashFn(6, true), 2..=2),
        "hashtext" => (HashFn(7, false), 1..=1),
        "hashtextextended" => (HashFn(7, true), 2..=2),
        "hashbpchar" => (HashFn(8, false), 1..=1),
        "hashbpcharextended" => (HashFn(8, true), 2..=2),
        "hashname" => (HashFn(9, false), 1..=1),
        "hashnameextended" => (HashFn(9, true), 2..=2),
        "hashenum" => (HashFn(10, false), 1..=1),
        "hashenumextended" => (HashFn(10, true), 2..=2),
        "interval_hash" => (HashFn(11, false), 1..=1),
        "interval_hash_extended" => (HashFn(11, true), 2..=2),
        "area" => (GeoFn(0), 1..=3),
        // (circle_center and the others: center's own functions, by the type they take)
        "center" | "circle_center" | "box_center" => (GeoFn(1), 1..=3),
        "radius" => (GeoFn(2), 1..=3),
        "diameter" => (GeoFn(3), 1..=3),
        "width" => (GeoFn(4), 1..=3),
        "height" => (GeoFn(5), 1..=3),
        "npoints" => (GeoFn(6), 1..=3),
        "isopen" => (GeoFn(7), 1..=3),
        "isclosed" => (GeoFn(8), 1..=3),
        "popen" => (GeoFn(9), 1..=3),
        "pclose" => (GeoFn(10), 1..=3),
        "slope" => (GeoFn(11), 1..=3),
        "isvertical" => (GeoFn(12), 1..=3),
        "ishorizontal" => (GeoFn(13), 1..=3),
        "bound_box" => (GeoFn(14), 1..=3),
        "diagonal" => (GeoFn(15), 1..=3),
        "point" => (GeoFn(16), 1..=3),
        "box" => (GeoFn(17), 1..=3),
        "lseg" => (GeoFn(18), 1..=3),
        "circle" => (GeoFn(19), 1..=3),
        "polygon" => (GeoFn(20), 1..=3),
        "path" => (GeoFn(21), 1..=3),
        "line" => (GeoFn(22), 1..=3),
        "poly_center" => (GeoFn(23), 1..=1),
        "isparallel" => (GeoFn(24), 2..=2),
        "isperp" => (GeoFn(25), 2..=2),
        "__interpt_pp" => (GeoFn(31), 2..=2),
        "get_bit" => (BitFn(0), 2..=2),
        "set_bit" => (BitFn(1), 3..=3),
        "bit_count" => (BitFn(2), 1..=1),
        "xid8cmp" => (XidFn(0), 2..=2),
        "pg_snapshot_xmin" => (XidFn(1), 1..=1),
        "pg_snapshot_xmax" => (XidFn(2), 1..=1),
        "pg_visible_in_snapshot" => (XidFn(3), 2..=2),
        "txid_snapshot_xmin" => (XidFn(4), 1..=1),
        "txid_snapshot_xmax" => (XidFn(5), 1..=1),
        "txid_visible_in_snapshot" => (XidFn(6), 2..=2),
        "cashlarger" => (MoneyFn(0), 2..=2),
        "cashsmaller" => (MoneyFn(1), 2..=2),
        "cash_words" => (MoneyFn(2), 1..=1),
        "date_bin" => (DateBin, 3..=3),
        "date_add" => (DateAdd(false), 2..=3),
        "date_subtract" => (DateAdd(true), 2..=3),
        "parse_ident" => (ParseIdent, 1..=2),
        "ts_lexize" => (TsLexize, 2..=2),
        "normalize" => (Normalize, 1..=2),
        "is_normalized" => (IsNormalized, 1..=2),
        "booleq" => (BoolCmp(0), 2..=2),
        "boolne" => (BoolCmp(1), 2..=2),
        "boolgt" => (BoolCmp(2), 2..=2),
        "boollt" => (BoolCmp(3), 2..=2),
        "boolge" => (BoolCmp(4), 2..=2),
        "boolle" => (BoolCmp(5), 2..=2),
        // bool_and's and bool_or's transition functions, strict as theirs are
        "booland_statefunc" => (BoolCmp(6), 2..=2),
        "boolor_statefunc" => (BoolCmp(7), 2..=2),
        "float8_accum" => (FloatAccum(0), 2..=2),
        "float8_regr_accum" => (FloatAccum(1), 3..=3),
        "float8_combine" => (FloatAccum(2), 2..=2),
        "float8_regr_combine" => (FloatAccum(3), 2..=2),
        "isempty" => (RangeFlag(0), 1..=1),
        "lower_inc" => (RangeFlag(1), 1..=1),
        "upper_inc" => (RangeFlag(2), 1..=1),
        "lower_inf" => (RangeFlag(3), 1..=1),
        "upper_inf" => (RangeFlag(4), 1..=1),
        "range_merge" => (RangeMerge, 1..=2),
        // multirange(r): the multirange of r's type (0 until the binder knows it: see `range_rewrite`)
        "multirange" => (MultiNew(0), 1..=1),
        n if super::range::multi_ctor(n).is_some() => (MultiNew(super::range::multi_ctor(n).unwrap_or(0)), 0..=MANY),
        n if super::range::by_name(n).is_some() => (RangeNew(super::range::by_name(n).unwrap_or(0)), 2..=3),
        "octet_length" => (OctetLength, 1..=1),
        "ascii" => (Ascii, 1..=1),
        "chr" => (Chr, 1..=1),
        "starts_with" => (StartsWith, 2..=2),
        "ceil" => (Ceil, 1..=1),
        "ceiling" => (Ceiling, 1..=1),
        "floor" => (Floor, 1..=1),
        "trunc" => (Trunc, 1..=2),
        "sqrt" => (Sqrt, 1..=1),
        "cbrt" => (Cbrt, 1..=1),
        "erf" => (Erf, 1..=1),
        "erfc" => (Erfc, 1..=1),
        "exp" => (Exp, 1..=1),
        "ln" => (Ln, 1..=1),
        "log" => (Log, 1..=2),
        "log10" => (Log10, 1..=1),
        "power" | "pow" => (Power, 2..=2),
        "mod" => (Mod, 2..=2),
        "sign" => (Sign, 1..=1),
        "isfinite" => (IsFinite, 1..=1),
        "justify_hours" => (JustifyHours, 1..=1),
        "justify_days" => (JustifyDays, 1..=1),
        "justify_interval" => (JustifyInterval, 1..=1),
        "degrees" => (Degrees, 1..=1),
        "radians" => (Radians, 1..=1),
        "sin" => (Sin, 1..=1),
        "cos" => (Cos, 1..=1),
        "tan" => (Tan, 1..=1),
        "cot" => (Cot, 1..=1),
        "asin" => (Asin, 1..=1),
        "acos" => (Acos, 1..=1),
        "atan" => (Atan, 1..=1),
        "atan2" => (Atan2, 2..=2),
        "sinh" => (Sinh, 1..=1),
        "sind" => (Sind, 1..=1),
        "cosd" => (Cosd, 1..=1),
        "tand" => (Tand, 1..=1),
        "cotd" => (Cotd, 1..=1),
        "asind" => (Asind, 1..=1),
        "acosd" => (Acosd, 1..=1),
        "atand" => (Atand, 1..=1),
        "atan2d" => (Atan2d, 2..=2),
        "asinh" => (Asinh, 1..=1),
        "acosh" => (Acosh, 1..=1),
        "atanh" => (Atanh, 1..=1),
        "scale" => (Scale, 1..=1),
        "min_scale" => (MinScale, 1..=1),
        "trim_scale" => (TrimScale, 1..=1),
        "cosh" => (Cosh, 1..=1),
        "tanh" => (Tanh, 1..=1),
        "div" => (Div, 2..=2),
        "numeric_inc" => (NumericInc, 1..=1),
        "gcd" => (Gcd, 2..=2),
        "lcm" => (Lcm, 2..=2),
        "factorial" => (Factorial, 1..=1),
        "width_bucket" => (WidthBucket, 2..=4),
        "to_hex" => (ToHex, 1..=1),
        "float4send" => (FloatSend(true), 1..=1),
        "float8send" => (FloatSend(false), 1..=1),
        "greatest" => (Greatest, 1..=MANY),
        "least" => (Least, 1..=MANY),
        "random" => (Random, 0..=0),
        "random_normal" => (RandomNormal, 0..=2),
        "setseed" => (SetSeed, 1..=1),
        "pi" => (Pi, 0..=0),
        "nullif" => (Nullif, 2..=2),
        "make_date" => (MakeDate, 3..=3),
        "make_time" => (MakeTime, 3..=3),
        "make_timestamp" => (MakeTimestamp, 6..=6),
        "make_timestamptz" => (MakeTimestampTz, 6..=7),
        "make_interval" => (MakeInterval, 0..=7),
        "grouping" => (Grouping, 1..=MANY),
        "array_append" => (ArrayAppend, 2..=2),
        "array_prepend" => (ArrayPrepend, 2..=2),
        "array_cat" => (ArrayCat, 2..=2),
        "array_remove" => (ArrayRemove, 2..=2),
        "array_replace" => (ArrayReplace, 3..=3),
        "array_position" => (ArrayPosition, 2..=3),
        "array_positions" => (ArrayPositions, 2..=2),
        "array_dims" => (ArrayDims, 1..=1),
        "array_ndims" => (ArrayNdims, 1..=1),
        "array_fill" => (ArrayFill, 2..=3),
        "trim_array" => (TrimArray, 2..=2),
        "array_shuffle" => (ArrayShuffle, 1..=1),
        "array_sample" => (ArraySample, 2..=2),
        "to_timestamp" => (ToTimestamp, 1..=2),
        "to_date" => (ToDate, 2..=2),
        // jsonb's (json's own are `JsonFn`: see above)
        "jsonb_build_object" => (JsonBuildObject, 0..=MANY),
        "jsonb_build_array" => (JsonBuildArray, 0..=MANY),
        "__json_ctor" => (JsonCtor, 1..=MANY),
        "__json_format" => (JsonFormatted, 4..=4),
        "jsonb_object" => (JsonObjectOf, 1..=2),
        "__is_json" => (IsJson, 2..=2),
        "to_jsonb" => (ToJson, 1..=1),
        "jsonb_typeof" => (JsonTypeof, 1..=1),
        "jsonb_array_length" => (JsonArrayLength, 1..=1),
        "jsonb_extract_path" => (JsonExtractPath, 2..=MANY),
        "jsonb_extract_path_text" => (JsonExtractPathText, 2..=MANY),
        "jsonb_path_exists" => (JsonPath(PathFn::Exists, false), 2..=4),
        "jsonb_path_exists_tz" => (JsonPath(PathFn::Exists, true), 2..=4),
        "jsonb_path_match" => (JsonPath(PathFn::Match, false), 2..=4),
        "jsonb_path_match_tz" => (JsonPath(PathFn::Match, true), 2..=4),
        "jsonb_path_query_array" => (JsonPath(PathFn::QueryArray, false), 2..=4),
        "jsonb_path_query_array_tz" => (JsonPath(PathFn::QueryArray, true), 2..=4),
        "jsonb_path_query_first" => (JsonPath(PathFn::QueryFirst, false), 2..=4),
        "jsonb_path_query_first_tz" => (JsonPath(PathFn::QueryFirst, true), 2..=4),
        "jsonb_set" => (JsonSet, 3..=4),
        "jsonb_set_lax" => (JsonSetLax, 3..=5),
        "jsonb_insert" => (JsonInsert, 3..=4),
        "jsonb_pretty" => (JsonPretty, 1..=1),
        "jsonb_strip_nulls" => (JsonStripNulls, 1..=1),
        "jsonb_contains" => (JsonOpFn(0), 2..=2),
        "jsonb_contained" => (JsonOpFn(1), 2..=2),
        "jsonb_exists" => (JsonOpFn(2), 2..=2),
        "jsonb_exists_any" => (JsonOpFn(3), 2..=2),
        "jsonb_exists_all" => (JsonOpFn(4), 2..=2),
        "jsonb_concat" => (JsonOpFn(5), 2..=2),
        "jsonb_delete" => (JsonOpFn(6), 2..=2),
        "jsonb_delete_path" => (JsonDeletePath, 2..=2),
        "pg_column_size" => (ColumnSize, 1..=1),
        "age" => (Age, 1..=2),
        "to_char" => (ToChar, 2..=2),
        "md5" => (Md5, 1..=1),
        "encode" => (Encode, 2..=2),
        "decode" => (Decode, 2..=2),
        "sha224" => (Sha224, 1..=1),
        "to_number" => (ToNumber, 2..=2),
        "sha256" => (Sha256, 1..=1),
        "sha384" => (Sha384, 1..=1),
        "sha512" => (Sha512, 1..=1),
        "get_byte" => (GetByte, 2..=2),
        "set_byte" => (SetByte, 3..=3),
        "bit_length" => (BitLength, 1..=1),
        "convert_to" => (ConvertTo, 2..=2),
        "convert_from" => (ConvertFrom, 2..=2),
        "overlay" => (Overlay, 3..=4),
        "host" => (NetHost, 1..=1),
        "abbrev" => (NetAbbrev, 1..=1),
        "broadcast" => (NetBroadcast, 1..=1),
        "netmask" => (NetNetmask, 1..=1),
        "hostmask" => (NetHostmask, 1..=1),
        "network" => (NetNetwork, 1..=1),
        "masklen" => (NetMasklen, 1..=1),
        "family" => (NetFamily, 1..=1),
        "set_masklen" => (NetSetMasklen, 2..=2),
        "inet_same_family" => (NetSameFamily, 2..=2),
        "inet_merge" => (NetMerge, 2..=2),
        "macaddr8_set7bit" => (NetSet7bit, 1..=1),
        "inet" => (NetInet, 1..=1),
        "cidr" => (NetCidr, 1..=1),
        "macaddr" => (NetMacaddr, 1..=1),
        "macaddr8" => (NetMacaddr8, 1..=1),
        "regexp_replace" => (RegexpReplace, 3..=6),
        "regexp_match" => (RegexpMatch, 2..=3),
        "regexp_like" => (RegexpLike, 2..=3),
        "similar_to_escape" => (SimilarEscape, 1..=2),
        "to_tsvector" => (ToTsvector, 1..=2),
        "jsonb_to_tsvector" | "json_to_tsvector" => (JsonToTsvector, 2..=3),
        "to_tsquery" => (ToTsquery, 1..=2),
        "plainto_tsquery" => (PlaintoTsquery, 1..=2),
        "phraseto_tsquery" => (PhrasetoTsquery, 1..=2),
        "websearch_to_tsquery" => (WebsearchToTsquery, 1..=2),
        "ts_rank" => (TsRank, 2..=4),
        "ts_rank_cd" => (TsRankCd, 2..=4),
        "xmlconcat" => (XmlConcat, 1..=usize::MAX),
        "xmlcomment" => (XmlComment, 1..=1),
        "xpath" => (Xpath, 2..=3),
        "xpath_exists" => (XpathExists, 2..=3),
        "xml_is_well_formed" => (XmlWellFormed(0), 1..=1),
        "xml_is_well_formed_content" => (XmlWellFormed(1), 1..=1),
        "xml_is_well_formed_document" => (XmlWellFormed(2), 1..=1),
        "ts_headline" => (TsHeadline, 2..=4),
        "setweight" => (Setweight, 2..=3),
        "ts_delete" => (TsDelete, 2..=2),
        "ts_filter" => (TsFilter, 2..=2),
        "array_to_tsvector" => (ArrayToTsvector, 1..=1),
        "tsvector_to_array" => (TsvectorToArray, 1..=1),
        "numnode" => (NumNode, 1..=1),
        "querytree" => (QueryTree, 1..=1),
        "tsquery_phrase" => (TsqueryPhrase, 2..=3),
        "tsquery_and" => (TsqueryAnd, 2..=2),
        "tsquery_or" => (TsqueryOr, 2..=2),
        "tsquery_not" => (TsqueryNot, 1..=1),
        "tsquery_cmp" => (TsqueryCmp, 2..=2),
        "tsq_mcontains" => (TsqueryContains, 2..=2),
        "ts_rewrite" => (TsRewrite, 3..=3),
        "strip" => (Strip, 1..=1),
        "tsvector_concat" => (TsConcat, 2..=2),
        "string_to_array" => (StringToArray, 2..=3),
        "array_to_string" => (ArrayToString, 2..=3),
        "num_nulls" => (NumNulls, 1..=MANY),
        "num_nonnulls" => (NumNonnulls, 1..=MANY),
        "pg_encoding_to_char" => (EncodingToChar, 1..=1),
        "pg_char_to_encoding" => (CharToEncoding, 1..=1),
        "cardinality" => (Cardinality, 1..=1),
        "array_length" => (ArrayLength, 2..=2),
        "array_lower" => (ArrayLower, 2..=2),
        "array_upper" => (ArrayUpper, 2..=2),
        // a point's coordinates, geography too: what spatial indexes keep (see `create_index`)
        "_st_x" => (GeoX, 1..=1),
        "_st_y" => (GeoY, 1..=1),
        "st_makepoint" => (StMakePoint, 2..=2),
        "st_point" => (StPoint, 2..=3),
        "st_setsrid" => (StSetSrid, 2..=2),
        "st_srid" => (StSrid, 1..=1),
        "st_geomfromtext" => (StGeomFromText, 1..=2),
        "st_geometryfromtext" => (StGeomFromText, 1..=2),
        "st_geomfromewkt" => (StGeomFromText, 1..=1),
        "st_geogfromtext" => (StGeogFromText, 1..=1),
        "st_geographyfromtext" => (StGeogFromText, 1..=1),
        "st_geomfromgeojson" => (StGeomFromGeoJson, 1..=1),
        "st_astext" => (StAsText, 1..=2),
        "st_aswkt" => (StAsText, 1..=2),
        "st_asewkt" => (StAsEwkt, 1..=1),
        "st_asgeojson" => (StAsGeoJson, 1..=3),
        "st_x" => (StX, 1..=1),
        "st_y" => (StY, 1..=1),
        "st_distance" => (StDistance, 2..=3),
        "st_dwithin" => (StDWithin, 3..=4),
        "st_distancesphere" => (StDistanceSphere, 2..=2),
        "st_distance_sphere" => (StDistanceSphere, 2..=2),
        "regexp_count" => (RegexpCount, 2..=4),
        "regexp_substr" => (RegexpSubstr, 2..=6),
        "regexp_instr" => (RegexpInstr, 2..=7),
        "regexp_split_to_array" => (RegexpSplit, 2..=3),
        "format" => (Format, 1..=MANY),
        "quote_ident" => (QuoteIdent, 1..=1),
        "quote_literal" => (QuoteLiteral, 1..=1),
        "quote_nullable" => (QuoteNullable, 1..=1),
        _ => return super::pgcrypto::lookup(name),
    })
}

/// The names `lookup` knows, for error messages.
pub(super) const NAMES: &str = "st_makepoint, st_point, st_setsrid, st_srid, st_geomfromtext, st_geogfromtext, st_geomfromgeojson, st_astext, st_asewkt, st_asgeojson, st_x, st_y, st_distance, st_dwithin, st_distancesphere, to_tsvector, to_tsquery, plainto_tsquery, phraseto_tsquery, websearch_to_tsquery, ts_rank, setweight, strip, concat, similar_to_escape, concat_ws, substring, substr, position, strpos, replace, trim, btrim, ltrim, rtrim, \
    left, right, lpad, rpad, repeat, reverse, translate, unistr, sha224, to_number, split_part, initcap, octet_length, ascii, chr, starts_with, ceil, floor, \
    trunc, sqrt, exp, ln, log, power, mod, sign, isfinite, justify_hours, justify_days, justify_interval, degrees, radians, sin, cos, tan, cot, sind, cosd, tand, cotd, asind, acosd, atand, atan2d, asinh, acosh, atanh, scale, min_scale, trim_scale, asin, acos, atan, atan2, sinh, cosh, tanh, div, gcd, lcm, factorial, width_bucket, to_hex, greatest, least, random, setseed, pi, nullif, make_date, \
    to_timestamp, to_date, make_time, make_timestamp, make_timestamptz, \
    jsonb_build_object, jsonb_build_array, to_jsonb, jsonb_typeof, jsonb_array_length, jsonb_extract_path(_text), \
    jsonb_set, jsonb_set_lax, jsonb_insert, jsonb_pretty, jsonb_strip_nulls (and their json_ names), age, to_char, md5, regexp_replace, regexp_match, regexp_like, \
    regexp_count, regexp_substr, regexp_instr, regexp_split_to_array, format, quote_ident, quote_literal, quote_nullable";

/// The names a call of a json function keeps for its column (see `Scalar::Spelled`): the jsonb_ ones of functions
/// json and jsonb share here, the ones only jsonb has, and the other names of to_json. The last is an operator's.
pub(super) const SPELLED: &[&str] = &[
    "jsonb_build_object",
    "jsonb_build_array",
    "jsonb_object",
    "to_jsonb",
    "row_to_json",
    "array_to_json",
    "jsonb_typeof",
    "jsonb_array_length",
    "jsonb_extract_path",
    "jsonb_extract_path_text",
    "jsonb_path_exists",
    "jsonb_path_exists_tz",
    "jsonb_path_match",
    "jsonb_path_match_tz",
    "jsonb_path_query_array",
    "jsonb_path_query_array_tz",
    "jsonb_path_query_first",
    "jsonb_path_query_first_tz",
    "jsonb_set",
    "jsonb_set_lax",
    "jsonb_insert",
    "jsonb_pretty",
    "jsonb_strip_nulls",
    "json_to_tsvector",
    "jsonb_contains",
    "jsonb_contained",
    "jsonb_exists",
    "jsonb_exists_any",
    "jsonb_exists_all",
    "jsonb_concat",
    "jsonb_delete",
    "jsonb_delete_path",
    "?column?",
    // (var_samp and stddev_samp under the names they were called by)
    "variance",
    "stddev",
    // (xmlagg: string_agg made XML)
    "xmlagg",
    // (SQL/JSON's, made of other calls: JSON_ARRAY of a query, and the aggregates)
    "json_array",
    "json_arrayagg",
    "json_objectagg",
    // (JSON_OBJECT and JSON_ARRAY RETURNING bytea: their text's bytes, under the constructor's name)
    "json_object",
];

/// The place of `name` in `SPELLED`.
pub(super) fn spelled(name: &str) -> Option<u8> {
    SPELLED.iter().position(|n| *n == name).map(|i| i as u8)
}

/// Its result type, from its arguments' (`first`: the first argument's that has one).
pub(super) fn result_type(f: Scalar, first: Option<Type>) -> Type {
    use Scalar::*;
    let f = match f {
        Ceiling => Ceil,
        Log10 => Log,
        Substring => Substr,
        f => f,
    };
    match f {
        Spelled(_) => first.unwrap_or(Type::Json),
        ColumnSize => Type::Int,
        Mask(_) => Type::Text,
        JsonOpFn(5 | 6) => Type::Json,
        JsonOpFn(_) => Type::Bool,
        f if super::pgcrypto::ours(f) => super::pgcrypto::result_type(f),
        Strpos | Position | OctetLength | Ascii | Grouping | GetByte | BitLength | CharToEncoding => Type::Int,
        Decode | Sha224 | Sha256 | Sha384 | Sha512 | SetByte | ConvertTo => Type::Bytea,
        // a bytea's part, or its bytea with another in the middle, is a bytea
        Substr | Overlay if first == Some(Type::Bytea) => Type::Bytea,
        StartsWith | BoolCmp(_) | IsNormalized | IsJson => Type::Bool,
        FloatAccum(_) => Type::Array(Type::Float.oid()),
        Float4Numeric => Type::Numeric,
        Float4Text => Type::Text,
        BoolArrayText => Type::Array(25),
        // x::"char"
        Char => Type::Letter,
        Float4Digits | Float4Round | Float4Acc => Type::Float4,
        PointNew => Type::Point,
        MoneyOp(0) | MoneyFn(0 | 1) => Type::Money,
        MoneyOp(1) => Type::Float,
        MoneyOp(_) => Type::Bool,
        MoneyFn(3) => Type::Numeric,
        SortKey => Type::Text,
        HashFn(_, true) => Type::BigInt,
        HashFn(_, false) => Type::Int,
        GeoOp => Type::Bool,
        GeoOpF => Type::Float,
        GeoOpV => Type::Text,
        GeoFn(k) => super::geomfn::result_type(k),
        TimeTzFn(0..=5) => Type::Bool,
        TimeTzFn(8) => Type::Time,
        TimeTzFn(11) => Type::Numeric,
        TimeTzFn(12) => Type::TimestampTz,
        TimeTzFn(_) => Type::TimeTz,
        StatReset => Type::Void,
        ArrayNest => first.unwrap_or(Type::Array(25)),
        TablespaceLocation => Type::Text,
        RecordImage => Type::Bool,
        IntArr(k) => intarray::result_type(k),
        Hstore(k) => super::hstore::result_type(k),
        JsonFn(k) => super::jsontext::result_type(k),
        Ltree(k) => super::ltree::result_type(k),
        XidFn(0) => Type::Int,
        XidFn(1 | 2) => Type::Xid8,
        XidFn(4 | 5) => Type::BigInt,
        XidFn(_) => Type::Bool,
        BitOp(_) | BitFit => Type::Text,
        BitToInt => Type::Int,
        BitFn(1) if first == Some(Type::Bytea) => Type::Bytea,
        BitFn(1) => Type::Text,
        BitFn(_) => Type::Int,
        MoneyFn(_) => Type::Text,
        BinaryCast => Type::Text, // (typed by the cast the binder made of it: see `Binder::go_cast`)
        LsnFn(0 | 3) => Type::Numeric,
        LsnFn(_) => Type::PgLsn,
        DateBin => Type::Timestamp,
        TimePrec => first.unwrap_or(Type::Timestamp),
        DateAdd(_) => Type::TimestampTz,
        Normalize => Type::Text,
        RangeNew(oid) => Type::Range(oid),
        MultiNew(oid) => Type::Multirange(super::range::multi_of(oid).unwrap_or(oid)),
        RangeFlag(_) => Type::Bool,
        RangeOp => Type::Bool,
        RangeSetOp => first.unwrap_or(Type::Text),
        RangeMerge => match first {
            Some(Type::Multirange(m)) => Type::Range(super::range::of_multi(m).unwrap_or(m)),
            t => t.unwrap_or(Type::Text),
        },
        RangeBound(_) => first
            .and_then(|t| match t {
                Type::Range(r) => super::range::subtype(r),
                Type::Multirange(m) => super::range::of_multi(m).and_then(super::range::subtype),
                _ => None,
            })
            .unwrap_or(Type::Text),
        NetHost | NetAbbrev => Type::Text,
        NetBroadcast | NetNetmask | NetHostmask | NetInet => Type::Inet,
        NetNetwork | NetMerge | NetCidr => Type::Cidr,
        NetMasklen | NetFamily => Type::Int,
        NetSameFamily => Type::Bool,
        NetSetMasklen => first.unwrap_or(Type::Inet),
        NetSet7bit | NetMacaddr8 => Type::MacAddr8,
        NetMacaddr => Type::MacAddr,
        Power | Sqrt | Exp | Ln | Log if first == Some(Type::Numeric) => Type::Numeric,
        Sqrt | Cbrt | Erf | Erfc | Exp | Ln | Log | Power | Random | RandomNormal | Pi => Type::Float,
        UuidV1(_) => Type::Uuid,
        Degrees | Radians | Sin | Cos | Tan | Cot | Asin | Acos | Atan | Atan2 | Sinh | Cosh | Tanh => Type::Float,
        Sind | Cosd | Tand | Cotd | Asind | Acosd | Atand | Atan2d | Asinh | Acosh | Atanh => Type::Float,
        Scale | MinScale => Type::Int,
        Similarity | WordSimilarity | StrictWordSimilarity => Type::Float,
        Levenshtein | LevenshteinLessEqual | Difference => Type::Int,
        Soundex | Metaphone | Dmetaphone | DmetaphoneAlt | Unaccent => Type::Text,
        ShowTrgm | DaitchMokotoff | ParseIdent | TsLexize => Type::Array(25),
        TrgmTest => Type::Bool,
        TrgmDist => Type::Float,
        ToNumber => Type::Numeric,
        TrimScale => Type::Numeric,
        Div | Factorial | NumericInc => Type::Numeric,
        JustifyHours | JustifyDays | JustifyInterval => Type::Interval,
        IsFinite => Type::Bool,
        Overlaps => Type::Bool,
        Gcd | Lcm => {
            if first == Some(Type::Numeric) {
                Type::Numeric
            } else if first == Some(Type::BigInt) {
                Type::BigInt
            } else {
                Type::Int
            }
        }
        WidthBucket => Type::Int,
        ToHex | JsonPretty => Type::Text,
        FloatSend(_) => Type::Bytea,
        // an integer is a double precision here, as Postgres has it (no integer versions)
        // (and so is a real's: they are float8's, which a real is converted to)
        Ceil | Floor | Trunc | Sign if first.is_some_and(|t| t.integral() || t == Type::Float4) => Type::Float,
        SetSeed => Type::Void,
        Ceil | Floor | Trunc | Mod | Sign | Greatest | Least | Nullif => first.unwrap_or(Type::Float),
        MakeDate => Type::Date,
        MakeTime => Type::Time,
        MakeTimestamp => Type::Timestamp,
        MakeTimestampTz => Type::TimestampTz,
        MakeInterval => Type::Interval,
        ToTimestamp => Type::TimestampTz,
        ToDate => Type::Date,
        JsonCtor | JsonFormatted => Type::JsonText,
        JsonBuildObject | JsonBuildArray | JsonObjectOf | ToJson | ArrayJson | JsonExtractPath | JsonSet
        | JsonStripNulls | JsonSetLax | JsonInsert | JsonDelete | JsonDeleteKeys | JsonDeletePath => Type::Json,
        JsonArrayLength => Type::Int,
        JsonPath(PathFn::Exists | PathFn::Match, _) => Type::Bool,
        JsonPath(..) => Type::Json,
        Age => Type::Interval,
        ArrayOf => Type::Array(first.unwrap_or(Type::Text).oid()),
        BigintOid => Type::Oid,
        RegexpMatch | RegexpSplit | StringToArray => Type::Array(25),
        RegexpLike => Type::Bool,
        ToTsvector | JsonToTsvector | Setweight | Strip | TsConcat | TsDelete | TsFilter | ArrayToTsvector => {
            Type::TsVector
        }
        TsvectorToArray => Type::Array(25),
        NumNode | TsqueryCmp => Type::Int,
        TsqueryContains => Type::Bool,
        XmlElement | XmlForest | XmlConcat | XmlPi | XmlRoot | XmlComment | XmlParse => Type::Xml,
        XmlSerialize => Type::Text,
        Xpath => Type::Array(142),
        XmlExistsFn | XpathExists | XmlWellFormed(_) | IsDocument => Type::Bool,
        TsHeadline => Type::Text,
        QueryTree | TsqueryPhrase | TsqueryAnd | TsqueryOr | TsqueryNot | TsRewrite => Type::TsQuery,
        ToTsquery | PlaintoTsquery | PhrasetoTsquery | WebsearchToTsquery => Type::TsQuery,
        // (real, as Postgres's: the f64 holds the float4's value, and goes out with a real's digits)
        TsRank | TsRankCd => Type::Float4,

        StMakePoint | StPoint | StGeomFromText | StGeomFromGeoJson => Type::Geometry,
        StGeogFromText => Type::Geography,
        StSetSrid => match first {
            Some(Type::Geography) => Type::Geography,
            _ => Type::Geometry,
        },
        StSrid => Type::Int,
        StX | StY | GeoX | GeoY | StDistance | StDistanceSphere => Type::Float,
        ArraySlice | ArrayAppend | ArrayCat | ArrayRemove | ArrayReplace | TrimArray | ArrayShuffle | ArraySample => {
            first.unwrap_or(Type::Text)
        }
        // (the element's type, whichever argument it is first)
        ArrayPrepend | ArrayFill => Type::Array(first.unwrap_or(Type::Text).oid()),
        ArrayPosition | ArrayNdims => Type::Int,
        ArrayPositions => Type::Array(Type::Int.oid()),
        ArraySet | ArraySetN => first.unwrap_or(Type::Text),
        ArrayAtN => match first {
            Some(t @ Type::Array(_)) => t.elem().unwrap_or(Type::Text),
            _ => Type::Text,
        },
        SampleKeep => Type::Bool,
        ArrayAt => match first {
            Some(t @ Type::Array(_)) => t.elem().unwrap_or(Type::Text),
            Some(Type::Json) => Type::Json,
            _ => Type::Text,
        },
        StDWithin => Type::Bool,
        RegexpCount | RegexpInstr | Cardinality | ArrayLength | ArrayLower | ArrayUpper | NumNulls | NumNonnulls => {
            Type::Int
        }
        _ => Type::Text,
    }
}

/// The array functions that take nulls in stride (a null array is an empty one, or the null
/// result, as each does in Postgres, and a null element is one to find, remove or replace).
fn array_function(f: Scalar, a: &[Value]) -> Result<Value> {
    use Scalar::*;
    use Value::*;
    let items = |v: &Value| -> Result<Option<Vec<Value>>> { super::array_values(v.clone()) };
    // equal for finding: a null matches a null, other values as they compare
    let number = |v: &Value| matches!(v, Int(_) | Float(_) | Numeric(_));
    let same = |p: &Value, q: &Value| match (p, q) {
        (Null, Null) => true,
        (Null, _) | (_, Null) => false,
        // an element of a numeric array (its text) against a number: as numbers, `1.0` is 1
        (Text(s), n) | (n, Text(s)) if number(n) => match super::coerce(Text(s.clone()), Type::Numeric) {
            Ok(x) => super::compare(&x, n).is_ok_and(std::cmp::Ordering::is_eq),
            Err(_) => s == &n.to_text(),
        },
        _ => super::compare(p, q).map_or_else(|_| p.to_text() == q.to_text(), std::cmp::Ordering::is_eq),
    };
    let array = |xs: Vec<Value>| Text(super::array_literal(&xs));
    use super::mdarray::{Arr, Dim};
    let arr = |v: &Value| super::mdarray::read(v);
    // (an element put on an array of more dimensions, which Postgres's array_append and array_prepend refuse)
    let one_dim = || err("22000", "argument must be empty or one-dimensional array");
    Ok(match f {
        // on the end of its one dimension, where it ends
        ArrayAppend => {
            let x = arr(&a[0])?.unwrap_or_else(Arr::empty);
            match x.dims.len() {
                0 => array(vec![a[1].clone()]),
                1 => {
                    let at = x.dims[0].hi().checked_add(1);
                    match at.filter(|&at| i32::try_from(at).is_ok()) {
                        Some(at) => x.set(&[at], a[1].clone())?.value(),
                        None => return err("22003", "integer out of range"),
                    }
                }
                _ => return one_dim(),
            }
        }
        // in front of its one dimension, which starts where it did
        ArrayPrepend => {
            let x = arr(&a[1])?.unwrap_or_else(Arr::empty);
            match x.dims.len() {
                0 => array(vec![a[0].clone()]),
                1 => {
                    let d = x.dims[0];
                    let mut elems = vec![a[0].clone()];
                    elems.extend(x.elems);
                    Arr { dims: vec![Dim { lo: d.lo, len: d.len + 1 }], elems }.value()
                }
                _ => return one_dim(),
            }
        }
        ArrayCat => match (arr(&a[0])?, arr(&a[1])?) {
            (None, None) => Null,
            (x, y) => super::mdarray::cat(x.unwrap_or_else(Arr::empty), y.unwrap_or_else(Arr::empty))?.value(),
        },
        // (where it starts kept)
        ArrayRemove => match arr(&a[0])? {
            None => Null,
            Some(x) if x.dims.len() > 1 => {
                return err(super::UNSUPPORTED, "removing elements from multidimensional arrays is not supported");
            }
            Some(x) => {
                let lo = x.dims.first().map_or(1, |d| d.lo);
                let kept: Vec<Value> = x.elems.into_iter().filter(|v| !same(v, &a[1])).collect();
                match kept.len() {
                    0 => Text("{}".into()),
                    len => Arr { dims: vec![Dim { lo, len }], elems: kept }.value(),
                }
            }
        },
        // (its dimensions kept)
        ArrayReplace => match arr(&a[0])? {
            None => Null,
            Some(mut x) => {
                for v in &mut x.elems {
                    if same(v, &a[1]) {
                        *v = a[2].clone();
                    }
                }
                x.value()
            }
        },
        ArrayPosition | ArrayPositions => {
            if arr(&a[0])?.is_some_and(|x| x.dims.len() > 1) {
                return err(super::UNSUPPORTED, "searching for elements in multidimensional arrays is not supported");
            }
            let Some(xs) = items(&a[0])? else { return Ok(Null) };
            let low = super::array_lower(&a[0]);
            let from = match a.get(2) {
                Some(Null) => return err("22004", "initial position must not be null"),
                Some(v) => int(v)?,
                None => low,
            };
            let hits = xs.iter().enumerate().filter(|(i, v)| low + *i as i64 >= from && same(v, &a[1]));
            if f == ArrayPositions {
                let at: Vec<Value> = hits.map(|(i, _)| Int(low + i as i64)).collect();
                return Ok(array(at));
            }
            hits.map(|(i, _)| Int(low + i as i64)).next().unwrap_or(Null)
        }
        // array_fill(value, dimensions [, lower bounds])
        _ => super::mdarray::fill(&a[0], &a[1], a.get(2))?.value(),
    })
}

/// Bytes as UTF-8 text, as Postgres reads text (a zero byte is none): else its error, which shows the bad sequence as
/// `report_invalid_encoding` does, as many bytes as its first says it has.
fn utf8_text(b: &[u8]) -> Result<String> {
    let valid = std::str::from_utf8(b).map_or_else(|e| e.valid_up_to(), |_| b.len());
    let at = b[..valid].iter().position(|&c| c == 0).unwrap_or(valid);
    if at == b.len() {
        return Ok(String::from_utf8_lossy(b).into_owned());
    }
    let bad = &b[at..];
    let n = match bad[0] {
        c if c & 0xe0 == 0xc0 => 2,
        c if c & 0xf0 == 0xe0 => 3,
        c if c & 0xf8 == 0xf0 => 4,
        _ => 1,
    };
    let shown: Vec<String> = bad.iter().take(n).map(|c| format!("0x{c:02x}")).collect();
    err("22021", format!("invalid byte sequence for encoding \"UTF8\": {}", shown.join(" ")))
}

fn int(v: &Value) -> Result<i64> {
    match super::coerce(v.clone(), Type::BigInt)? {
        Value::Int(n) => Ok(n),
        _ => err(super::WRONG_TYPE, "expected an integer"),
    }
}

fn float(v: &Value) -> Result<f64> {
    match super::num(v.clone())? {
        Value::Int(n) => Ok(n as f64),
        Value::Float(f) => Ok(f),
        Value::Numeric(d) => Ok(d.to_f64()),
        v => err(super::WRONG_TYPE, format!("expected a number, not {}", v.kind())),
    }
}

/// A `real` result: the digits a float4 prints with (Chronos holds a real as a double precision).
fn real(x: f32) -> Value {
    Value::Float(x.to_string().parse().unwrap_or(f64::from(x)))
}

/// pg_trgm's operators: `kind` is the operator, the texts its operands. `%` and its kin test against the
/// threshold (`pg_trgm.similarity_threshold` 0.3, `word_similarity_threshold` 0.6,
/// `strict_word_similarity_threshold` 0.5), the `<->` family give distances.
fn trgm_op(kind: &str, a: &str, b: &str) -> Result<Value> {
    use textext::*;
    Ok(match kind {
        "%" => Value::Bool(pg(similarity_op(a, b, SIMILARITY_THRESHOLD))?),
        "<%" => Value::Bool(pg(word_similarity_op(a, b, WORD_SIMILARITY_THRESHOLD))?),
        "%>" => Value::Bool(pg(word_similarity_commutator_op(a, b, WORD_SIMILARITY_THRESHOLD))?),
        "<<%" => Value::Bool(pg(strict_word_similarity_op(a, b, STRICT_WORD_SIMILARITY_THRESHOLD))?),
        "%>>" => Value::Bool(pg(strict_word_similarity_commutator_op(a, b, STRICT_WORD_SIMILARITY_THRESHOLD))?),
        "<->" => real(pg(similarity_dist(a, b))?),
        "<<->" => real(pg(word_similarity_dist(a, b))?),
        "<->>" => real(pg(word_similarity_dist_commutator(a, b))?),
        "<<<->" => real(pg(strict_word_similarity_dist(a, b))?),
        "<->>>" => real(pg(strict_word_similarity_dist_commutator(a, b))?),
        _ => return err("42883", format!("operator does not exist: {kind}")),
    })
}

/// What a textext function gives back, with its error (a SQLSTATE and a message) as the SQL error.
fn pg<T>(r: std::result::Result<T, textext::Fail>) -> Result<T> {
    r.or_else(|(code, message)| err(code, message))
}

/// What quote_literal and quote_nullable quote: the value as text, as its cast to text writes it.
fn quoted_text(v: &Value) -> Result<String> {
    match v {
        Value::Net(n) => net::text(n),
        v => Ok(text(v)),
    }
}

fn text(v: &Value) -> String {
    match v {
        Value::Bool(b) => (if *b { "t" } else { "f" }).into(), // Postgres's output form, as concat shows it
        v => v.to_text(),
    }
}

/// Characters `set` doesn't hold, trimmed from `s`'s start and/or end.
fn trim(s: &str, set: &str, start: bool, end: bool) -> String {
    let set: Vec<char> = set.chars().collect();
    let mut t = s;
    if start {
        t = t.trim_start_matches(|c| set.contains(&c));
    }
    if end {
        t = t.trim_end_matches(|c| set.contains(&c));
    }
    t.into()
}

/// The most bytes a value built here may take: Postgres's MaxAllocSize (1 GB), past which it
/// refuses, as Postgres does, rather than try to allocate it.
pub(super) const MAX_BYTES: usize = (1 << 30) - 1;

/// `bytes`, if a value may be that long.
pub(super) fn fits(bytes: Option<usize>) -> Result<usize> {
    match bytes {
        Some(n) if n <= MAX_BYTES => Ok(n),
        _ => err("54000", "requested length too large"),
    }
}

/// About what one item of an array built here takes until it's printed: its text and value, and
/// its printed form.
pub(super) const ITEM: usize = 64 + std::mem::size_of::<Value>();

/// `s`'s characters, if they'd fit (4 bytes each): Postgres refuses the same, converting text
/// to wide characters for regular expressions.
/// A character in capitals as Postgres has it: one character for one (the single mapping of a
/// locale's `towupper`, not Unicode's full one that turns `ß` into `SS`).
pub(super) fn upper_char(c: char) -> char {
    let mut it = c.to_uppercase();
    match (it.next(), it.next()) {
        (Some(u), None) => u,
        // the letters with an iota subscript have a single capital (the titlecase letter); the
        // others (ß, ŉ, the ligatures) have none
        _ => match c as u32 {
            n @ (0x1f80..=0x1f87 | 0x1f90..=0x1f97 | 0x1fa0..=0x1fa7) => char::from_u32(n + 8).unwrap_or(c),
            0x1fb3 => '\u{1fbc}',
            0x1fc3 => '\u{1fcc}',
            0x1ff3 => '\u{1ffc}',
            _ => c,
        },
    }
}

/// A character in lower case, one for one (`İ` is `i`, and a capital sigma is `σ` wherever it is).
pub(super) fn lower_char(c: char) -> char {
    let mut it = c.to_lowercase();
    match (it.next(), it.next()) {
        (Some(l), None) => l,
        _ if c == '\u{130}' => 'i',
        _ => c,
    }
}

/// A text with its characters in capitals or in lower case (`upper`, `lower`).
pub(super) fn recase(s: &str, upper: bool) -> String {
    s.chars().map(|c| if upper { upper_char(c) } else { lower_char(c) }).collect()
}

/// What `unistr` and a `U&'...'` literal make of their text: `esc` and four hex digits, or `+`
/// and six, is a character (`short` also has `u` and four, `U` and eight, as `unistr` does);
/// `esc` twice is itself; a UTF-16 surrogate pair is one character.
pub(super) fn unescape(s: &str, esc: char, short: bool) -> std::result::Result<String, (&'static str, String)> {
    unescape_at(s, esc, short).map_err(|(code, message, _)| (code, message))
}

/// `unescape`, its error also saying where in `s` (in characters) it is, as Postgres points at it: an
/// escape that isn't one, or the place after a leading surrogate where its trailing one should be.
pub(super) fn unescape_at(
    s: &str,
    esc: char,
    short: bool,
) -> std::result::Result<String, (&'static str, String, usize)> {
    let cs: Vec<char> = s.chars().collect();
    let (mut out, mut i) = (String::new(), 0);
    let invalid = |at: usize| (super::SYNTAX, "invalid Unicode escape".to_string(), at);
    // the code point an escape spells, from just after its escape character: its value and length
    let code = |i: usize| -> Option<(u32, usize)> {
        let (skip, n) = match cs.get(i) {
            Some('+') => (1, 6),
            Some('u') if short => (1, 4),
            Some('U') if short => (1, 8),
            _ => (0, 4),
        };
        let digits: String = cs.get(i + skip..i + skip + n)?.iter().collect();
        if !digits.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        Some((u32::from_str_radix(&digits, 16).ok()?, skip + n))
    };
    let pair = |at: usize| (super::SYNTAX, "invalid Unicode surrogate pair".to_string(), at);
    while i < cs.len() {
        if cs[i] != esc {
            out.push(cs[i]);
            i += 1;
        } else if cs.get(i + 1) == Some(&esc) {
            out.push(esc);
            i += 2;
        } else {
            let start = i;
            let (mut c, n) = code(i + 1).ok_or_else(|| invalid(start))?;
            i += 1 + n;
            if (0xd800..0xdc00).contains(&c) {
                // a leading surrogate needs an escape for the trailing one straight after
                let low = if cs.get(i) == Some(&esc) { code(i + 1) } else { None };
                match low {
                    Some((low, n)) if (0xdc00..0xe000).contains(&low) => {
                        c = 0x10000 + ((c - 0xd800) << 10) + (low - 0xdc00);
                        i += 1 + n;
                    }
                    _ => return Err(pair(i)),
                }
            } else if (0xdc00..0xe000).contains(&c) {
                return Err(pair(start));
            }
            match char::from_u32(c).filter(|c| *c != '\0') {
                Some(c) => out.push(c),
                None if short => return Err(("22023", format!("invalid Unicode code point: {c:04X}"), start)),
                None => return Err((super::SYNTAX, "invalid Unicode escape value".into(), start)),
            }
        }
    }
    Ok(out)
}

pub(super) fn chars(s: &str) -> Result<Vec<char>> {
    fits(s.chars().count().checked_mul(4))?;
    Ok(s.chars().collect())
}

/// `parts` joined by `sep`, refused past MAX_BYTES as it grows.
fn joined(parts: impl Iterator<Item = String>, sep: &str) -> Result<String> {
    let mut out = String::new();
    for (i, p) in parts.enumerate() {
        fits(out.len().checked_add(p.len()).and_then(|n| n.checked_add(if i > 0 { sep.len() } else { 0 })))?;
        if i > 0 {
            out += sep;
        }
        out += &p;
    }
    Ok(out)
}

/// An array's bound: Postgres's are integers (int4).
fn bound(b: i64) -> Result<i64> {
    match i32::try_from(b) {
        Ok(b) => Ok(i64::from(b)),
        Err(_) => err("22003", "array bound is out of integer range"),
    }
}

fn pad(s: &str, n: i64, fill: &str, left: bool) -> Result<String> {
    let chars: Vec<char> = s.chars().collect();
    let n = usize::try_from(n.max(0)).unwrap_or(usize::MAX);
    if chars.len() >= n || fill.is_empty() {
        return Ok(chars[..n.min(chars.len())].iter().collect());
    }
    fits(n.checked_mul(4))?; // as Postgres checks it: 4 bytes a character at most
    let need = n - chars.len();
    let pad: String = fill.chars().cycle().take(need).collect();
    Ok(if left { pad + s } else { s.to_string() + &pad })
}

/// MD5 (RFC 1321) as 32 hex digits, as Postgres's md5() gives it.
pub(super) fn md5(data: &[u8]) -> String {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14,
        20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6,
        10, 15, 21,
    ];
    let k: Vec<u32> = (0..64).map(|i| ((i as f64 + 1.0).sin().abs() * 4294967296.0) as u32).collect();
    let (mut a0, mut b0, mut c0, mut d0) = (0x67452301u32, 0xefcdab89u32, 0x98badcfeu32, 0x10325476u32);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend(((data.len() as u64).wrapping_mul(8)).to_le_bytes());
    for chunk in msg.chunks(64) {
        let w: Vec<u32> = chunk.chunks(4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).collect();
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(w[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    [a0, b0, c0, d0].iter().flat_map(|x| x.to_le_bytes()).map(|b| format!("{b:02x}")).collect()
}

/// A seeded generator: its xoroshiro128** state (Postgres's pg_prng), and how many row ids it has given (see
/// `new_id`).
pub(crate) type Seeded = (u64, u64, u64);

thread_local! {
    /// The running session's seeded generator (setseed, or a simulation world's seed), if any.
    /// While there is one, statements run on one core, so what random() gives each row depends
    /// on the seed and the data only (see `par_run`).
    pub(crate) static SEEDED: std::cell::Cell<Option<Seeded>> = const { std::cell::Cell::new(None) };
}

/// A generator seeded with `seed` as Postgres's pg_prng_seed seeds one (splitmix64 twice): the same seed gives the
/// same numbers, and setseed's the ones Postgres's random() gives.
pub(crate) fn seeded(seed: u64) -> Seeded {
    let mut at = seed;
    let mut next = || {
        at = at.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut x = at;
        x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        x ^ (x >> 31)
    };
    let (s0, s1) = (next(), next());
    // (all zeroes is xoroshiro's fixed point: pg_prng_seed_check's replacement)
    if s0 == 0 && s1 == 0 { (0x5851_f42d_4c95_7f2d, 0x1405_7b7e_f767_814f, 0) } else { (s0, s1, 0) }
}

/// splitmix64's finalizer: nearby seeds give unrelated states.
pub(crate) fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// xoroshiro128**, as pg_prng has it: the next 64 bits, the state moved on.
fn xoroshiro(s0: &mut u64, s1: &mut u64) -> u64 {
    let (a, x) = (*s0, *s1 ^ *s0);
    let v = a.wrapping_mul(5).rotate_left(7).wrapping_mul(9);
    *s0 = a.rotate_left(24) ^ x ^ (x << 16);
    *s1 = x.rotate_left(37);
    v
}

/// 64 random bits: from the session's seeded generator, else from a per-thread one seeded by the clock and the thread.
pub(crate) fn random_bits() -> u64 {
    use std::cell::Cell;
    thread_local! {
        static STATE: Cell<(u64, u64)> = Cell::new({
            use std::hash::{BuildHasher, Hasher};
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
            let (s0, s1, _) = seeded(h.finish());
            (s0, s1)
        });
    }
    match SEEDED.get() {
        Some((mut s0, mut s1, n)) => {
            let r = xoroshiro(&mut s0, &mut s1);
            SEEDED.set(Some((s0, s1, n)));
            r
        }
        None => STATE.with(|s| {
            let (mut s0, mut s1) = s.get();
            let r = xoroshiro(&mut s0, &mut s1);
            s.set((s0, s1));
            r
        }),
    }
}

/// A random number in [0, 1), as pg_prng_double makes one of 64 bits: their top 52.
pub(super) fn random() -> f64 {
    (random_bits() >> 12) as f64 / (1u64 << 52) as f64
}

/// random_normal(mean, stddev): pg_prng_double_normal's Box-Muller transform of two of random()'s numbers.
fn random_normal(mean: f64, stddev: f64) -> f64 {
    let (u1, u2) = (1.0 - random(), 1.0 - random());
    let z = (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).sin();
    stddev * z + mean
}

/// setseed(x): seeds the session's random(), as Postgres's pg_prng_fseed does (x from -1 to 1).
fn setseed(x: f64) -> Result<Value> {
    if !(-1.0..=1.0).contains(&x) {
        return err("22023", format!("setseed parameter {} is out of allowed range [-1,1]", float_g(x)));
    }
    let seed = (((1u64 << 52) - 1) as f64 * x) as i64;
    SEEDED.set(Some(seeded(seed as u64)));
    Ok(Value::Text(String::new())) // void
}

/// PostGIS's functions, which the binder tells whether their points are geographies.
pub(super) fn geo_kind(f: Scalar) -> bool {
    use Scalar::*;
    matches!(f, StX | StY | StDistance | StDWithin)
}

pub(super) fn geo_name(f: Scalar) -> &'static str {
    match f {
        Scalar::StX => "st_x",
        Scalar::StY => "st_y",
        Scalar::StDistance => "st_distance",
        _ => "st_dwithin",
    }
}

fn point(v: &Value) -> Result<super::geo::Point> {
    super::geo::Point::parse(&text(v)).map_err(super::geo_err)
}

fn geo(f: Scalar, a: &[Value]) -> Result<Value> {
    use super::geo::{self, Point};
    use Scalar::*;
    use Value::*;
    let digits = |i: usize, default: i64| -> Result<usize> {
        Ok(a.get(i).map(int).transpose()?.unwrap_or(default).clamp(0, 20) as usize)
    };
    Ok(match f {
        StMakePoint | StPoint => {
            let srid = a.get(2).map(int).transpose()?.unwrap_or(0) as u32;
            Text(Point { srid, x: float(&a[0])?, y: float(&a[1])? }.hex())
        }
        StSetSrid => {
            let p = Point { srid: int(&a[1])? as u32, ..point(&a[0])? };
            Text(p.hex())
        }
        StSrid => Int(point(&a[0])?.srid as i64),
        StGeomFromText => {
            let mut p = point(&a[0])?;
            if let Some(s) = a.get(1) {
                p.srid = int(s)? as u32;
            }
            Text(p.hex())
        }
        StGeogFromText => Text(point(&a[0])?.geography().map_err(super::geo_err)?.hex()),
        StGeomFromGeoJson => Text(Point::from_geojson(&text(&a[0])).map_err(super::geo_err)?.hex()),
        StAsText => Text(point(&a[0])?.wkt()),
        StAsEwkt => Text(point(&a[0])?.ewkt()),
        StAsGeoJson => Text(point(&a[0])?.geojson(digits(1, 9)?)),
        StX | GeoX => Float(point(&a[0])?.x),
        StY | GeoY => Float(point(&a[0])?.y),
        StDistance | StDWithin => {
            // [a, b, (distance,) use_spheroid, geography?], as the binder lays them out
            let geog = matches!(a.last(), Some(Bool(true)));
            let (p, q) = (point(&a[0])?, point(&a[1])?);
            let d = if geog {
                let (p, q) = (p.geography().map_err(super::geo_err)?, q.geography().map_err(super::geo_err)?);
                let spheroid = !matches!(a.get(if f == StDistance { 2 } else { 3 }), Some(Bool(false)));
                geo::geography_distance(&p, &q, spheroid)
            } else {
                geo::planar(&p, &q).map_err(super::geo_err)?
            };
            match f {
                StDistance => Float(d),
                _ => Bool(d <= float(&a[2])?),
            }
        }
        StDistanceSphere => {
            let (p, q) = (point(&a[0])?, point(&a[1])?);
            Float(geo::sphere(&p, &q))
        }
        _ => unreachable!(),
    })
}

/// The encodings Postgres numbers, in order (`pg_encoding_to_char`).
pub(super) const ENCODINGS: [&str; 42] = [
    "SQL_ASCII",
    "EUC_JP",
    "EUC_CN",
    "EUC_KR",
    "EUC_TW",
    "EUC_JIS_2004",
    "UTF8",
    "MULE_INTERNAL",
    "LATIN1",
    "LATIN2",
    "LATIN3",
    "LATIN4",
    "LATIN5",
    "LATIN6",
    "LATIN7",
    "LATIN8",
    "LATIN9",
    "LATIN10",
    "WIN1256",
    "WIN1258",
    "WIN866",
    "WIN874",
    "KOI8R",
    "WIN1251",
    "WIN1252",
    "ISO_8859_5",
    "ISO_8859_6",
    "ISO_8859_7",
    "ISO_8859_8",
    "WIN1250",
    "WIN1253",
    "WIN1254",
    "WIN1255",
    "WIN1257",
    "KOI8U",
    "SJIS",
    "BIG5",
    "GBK",
    "UHC",
    "GB18030",
    "JOHAB",
    "SHIFT_JIS_2004",
];

/// Text read as a "char" and written back (see `Scalar::Char`).
pub(super) fn one_byte(s: &str) -> String {
    let b = s.as_bytes();
    let octal = |c: u8| (b'0'..=b'7').contains(&c);
    let byte = if b.len() == 4 && b[0] == b'\\' && b[1..].iter().all(|c| octal(*c)) {
        ((b[1] - b'0') << 6).wrapping_add((b[2] - b'0') << 3).wrapping_add(b[3] - b'0')
    } else {
        b.first().copied().unwrap_or(0)
    };
    match byte {
        0 => String::new(),
        c if c >= 0x80 => format!("\\{c:03o}"),
        c => char::from(c).to_string(),
    }
}

/// The argument before a VARIADIC array concat, concat_ws or format takes whole (`concat(VARIADIC a)` of an
/// array not written out): text no value can be, as it holds a NUL.
pub(super) const SPREAD: &str = "\u{0}VARIADIC";

/// A call of concat, concat_ws or format with its VARIADIC array (after `SPREAD`) spread into its arguments, as
/// Postgres's functions of VARIADIC "any" take it: a null array makes concat and concat_ws null, and is no
/// arguments to format.
fn spread_call(f: Scalar, a: &[Value]) -> Result<Value> {
    let at = a.iter().position(|v| matches!(v, Value::Text(s) if s == SPREAD)).unwrap_or(a.len());
    let items = match a.get(at + 1) {
        None | Some(Value::Null) => None,
        Some(v) => super::array_values(v.clone())?,
    };
    let mut args = a[..at].to_vec();
    match items {
        Some(items) => args.extend(items),
        None if f == Scalar::Format => {}
        None => return Ok(Value::Null),
    }
    call(f, &args)
}

/// A value's bits as another type of the same width reads them (a binary-coercible cast's): integer and real are 4
/// bytes, bigint and double precision 8; a type of another representation is taken as it is.
fn binary_cast(a: &[Value]) -> Result<Value> {
    use Value::*;
    let (Some(v), Some(Int(from)), Some(Int(to))) = (a.first(), a.get(1), a.get(2)) else { return Ok(Null) };
    let bits: u64 = match (v, from) {
        (Null, _) => return Ok(Null),
        (Int(n), 23) => u64::from(*n as i32 as u32),
        (Int(n), 20) => *n as u64,
        (Float(f), 700) => u64::from((*f as f32).to_bits()),
        (Float(f), 701) => f.to_bits(),
        (v, _) => return Ok(v.clone()),
    };
    Ok(match to {
        23 => Int(i64::from(bits as u32 as i32)),
        20 => Int(bits as i64),
        700 => Float(f64::from(f32::from_bits(bits as u32))),
        701 => Float(f64::from_bits(bits)),
        _ => v.clone(),
    })
}

/// pg_lsn's arithmetic (pg_lsn_mi, pg_lsn_pli, pg_lsn_mii), and an LSN as the number it is: 0 `lsn - lsn`, a
/// numeric; 1 `lsn + numeric` and 2 `lsn - numeric`, an LSN (out of range below 0 or past 64 bits, refused for NaN);
/// 3 the LSN as a numeric (what min and max compare).
fn lsn_call(k: u8, a: &[Value]) -> Result<Value> {
    use Value::*;
    if a.contains(&Null) {
        return Ok(Null);
    }
    use super::decimal::Dec;
    let number = |v: &Value| -> Result<Dec> { Ok(Dec::new(i128::from(super::lsn_value(&v.to_text())?), 0)) };
    let exact = |r: std::result::Result<Dec, &'static str>| {
        r.map_err(|m| super::SqlError { code: super::decimal::sqlstate(m), message: m.into(), info: None })
    };
    match k {
        0 => Ok(Numeric(exact(Dec::sub(number(&a[0])?, number(&a[1])?))?)),
        3 => Ok(Numeric(number(&a[0])?)),
        _ => {
            let Numeric(d) = super::coerce(a[1].clone(), Type::Numeric)? else {
                return Ok(Null);
            };
            if d.is_nan() {
                let what = if k == 1 { "cannot add NaN to pg_lsn" } else { "cannot subtract NaN from pg_lsn" };
                return err("0A000", what);
            }
            let lsn = number(&a[0])?;
            let moved = exact(if k == 1 { Dec::add(lsn, d) } else { Dec::sub(lsn, d) })?;
            super::coerce(Numeric(moved), Type::PgLsn)
        }
    }
}

pub(super) fn call(f: Scalar, a: &[Value]) -> Result<Value> {
    use Scalar::*;
    use Value::*;
    let f = match f {
        Ceiling => Ceil,
        Log10 => Log,
        Substring => Substr,
        f => f,
    };
    if matches!(f, Concat | ConcatWs | Format | NumNulls | NumNonnulls)
        && a.iter().any(|v| matches!(v, Text(s) if s == SPREAD))
    {
        return spread_call(f, a);
    }
    // these take nulls in stride; every other function is null if any argument is
    match f {
        ArrayAppend | ArrayPrepend | ArrayCat | ArrayRemove | ArrayReplace | ArrayPosition | ArrayPositions
        | ArrayFill => return array_function(f, a),
        Overlaps => return overlaps(a),
        MoneyOp(_) | MoneyFn(_) => return super::money::call(f, a),
        LsnFn(k) => return lsn_call(k, a),
        BinaryCast => return binary_cast(a),
        XidFn(_) => return super::xid::call(f, a),
        TimeTzFn(_) => return super::timetz::call(f, a),
        StatReset => return super::stmtstats::reset(a, super::stmtstats::world_oids),
        Mask(m) => return super::mask::apply(m, a),
        Spelled(_) => return Ok(a.first().cloned().unwrap_or(Null)),
        // (read as their operators: see `builtin_call`)
        JsonOpFn(_) => return err("42883", "a jsonb operator's function is read as the operator"),
        // (the default ones, pg_default and pg_global, and 0 for the database's: in the data folder; there are no
        // others, whose link Postgres would fail to find)
        TablespaceLocation => {
            return match &a[0] {
                Null => Ok(Null),
                Int(0 | 1663 | 1664) => Ok(Text(String::new())),
                v => {
                    let oid = v.to_text().parse::<i64>().map_or_else(|_| v.to_text(), |o| (o as u32).to_string());
                    err("58P01", format!("could not stat file \"pg_tblspc/{oid}\": No such file or directory"))
                }
            };
        }
        RecordImage => return super::record_image(a),
        IntArr(_) => return intarray::call(f, a),
        Hstore(_) => return super::hstore::call(f, a),
        // (each takes nulls as Postgres's does: json_build_object's values, a strict one's argument)
        JsonFn(k) => return super::jsontext::call(k, a),
        Ltree(_) => return super::ltree::call(f, a),
        GeoOp | GeoOpF | GeoOpV | GeoFn(_) => return super::geomfn::call(f, a),
        HashFn(..) => return super::hashfn::call(f, a),
        SortKey => {
            if a[0] == Null {
                return Ok(Null);
            }
            return match &a[1] {
                Int(-1) => super::money::parse(&a[0].to_text()).map(Int),
                Int(-2) => super::timetz::sort_key(&a[0].to_text()).map(Int),
                Int(-3) => Ok(Text(super::ltree::sort_key(&a[0].to_text()))),
                Int(-4) => Ok(Int(super::tid_key(&a[0].to_text()))),
                // (an LSN: its 64 bits in hex, 16 digits, which sort as the numbers do)
                Int(-7) => super::lsn_value(&a[0].to_text()).map(|n| Text(format!("{n:016X}"))),
                Int(-5) => Ok(super::array_sort_key(&a[0], true)),
                Int(-6) => Ok(super::array_sort_key(&a[0], false)),
                Int(o) => super::range::sort_key(&a[0].to_text(), *o as u32).map(Text),
                _ => Ok(a[0].clone()),
            };
        }
        BitOp(_) | BitFit | BitToInt | BitFn(_) => return super::bit::call(f, a),
        RangeNew(_) | MultiNew(_) | RangeOp | RangeSetOp | RangeBound(_) | RangeFlag(_) | RangeMerge => {
            return super::range::call(f, a);
        }
        Concat => return Ok(Text(joined(a.iter().filter(|v| **v != Null).map(text), "")?)),
        NumNulls | NumNonnulls => {
            let nulls = a.iter().filter(|v| **v == Null).count();
            return Ok(Int(if f == NumNulls { nulls } else { a.len() - nulls } as i64));
        }
        // the null string stands for a null item (else nulls are left out); a null array or separator gives null
        ArrayToString if a[0] != Null && a[1] != Null => {
            let sep = text(&a[1]);
            let null = a.get(2).filter(|v| **v != Null).map(text);
            let items = super::array_values(a[0].clone())?.unwrap_or_default();
            let shown = items.iter().filter_map(|v| match v {
                Null => null.clone(),
                v => Some(text(v)),
            });
            return Ok(Text(joined(shown, &sep)?));
        }
        // string_to_array(s, NULL) splits into characters
        StringToArray if a[0] != Null => {
            let s = text(&a[0]);
            if s.is_empty() {
                return Ok(Text("{}".into()));
            }
            // each item costs far more than its text: refused before it's built past MAX_BYTES
            let n = match &a[1] {
                Null => s.chars().count(),
                d if text(d).is_empty() => 1,
                d => s.matches(text(d).as_str()).count() + 1,
            };
            fits(n.checked_mul(ITEM).and_then(|m| m.checked_add(s.len())))?;
            let parts: Vec<String> = match &a[1] {
                Null => s.chars().map(String::from).collect(),
                d if text(d).is_empty() => vec![s],
                d => s.split(text(d).as_str()).map(String::from).collect(),
            };
            let null = a.get(2).filter(|v| **v != Null).map(text);
            let items: Vec<Value> =
                parts.into_iter().map(|p| if Some(&p) == null.as_ref() { Null } else { Text(p) }).collect();
            return Ok(Text(super::array_literal(&items)));
        }
        ConcatWs => {
            if a[0] == Null {
                return Ok(Null);
            }
            let sep = text(&a[0]);
            return Ok(Text(joined(a[1..].iter().filter(|v| **v != Null).map(text), &sep)?));
        }
        Greatest | Least => {
            let mut best: Option<&Value> = None;
            for v in a.iter().filter(|v| **v != Null) {
                let better = match best {
                    None => true,
                    Some(b) => {
                        let o = super::compare(v, b)?;
                        if f == Greatest { o.is_gt() } else { o.is_lt() }
                    }
                };
                if better {
                    best = Some(v);
                }
            }
            return Ok(best.cloned().unwrap_or(Null));
        }
        Nullif => {
            return Ok(match (&a[0], &a[1]) {
                (Null, _) => Null,
                (x, Null) => x.clone(),
                (x, y) if super::compare(x, y)?.is_eq() => Null,
                (x, _) => x.clone(),
            });
        }
        JsonBuildObject => {
            if a.len() % 2 == 1 {
                return Err(super::SqlError {
                    code: "22023",
                    message: "argument list must have even number of elements".into(),
                    info: Some(Box::new(super::Violation {
                        hint: Some(
                            "The arguments of jsonb_build_object() must consist of alternating keys and values.".into(),
                        ),
                        ..Default::default()
                    })),
                });
            }
            let mut o = serde_json::Map::new();
            for (i, kv) in a.chunks(2).enumerate() {
                if kv[0] == Null {
                    return err("22004", format!("argument {}: key must not be null", 2 * i + 1));
                }
                // (an array or a row is made JSON before it gets here: none of them is a key)
                if matches!(kv[0], Json(_) | JsonText(_)) {
                    return err("22023", "key value must be scalar, not array, composite, or json");
                }
                o.insert(kv[0].to_text(), kv[1].to_json());
            }
            return Ok(Json(super::jsonb::shallow(serde_json::Value::Object(o))?));
        }
        JsonBuildArray => {
            return Ok(Json(super::jsonb::shallow(serde_json::Value::Array(a.iter().map(Value::to_json).collect()))?));
        }
        // (bound away where its type is known; read unbound, json of its text)
        JsonFormatted => return super::coerce(a[0].clone(), Type::JsonText),
        JsonCtor => {
            let flags = match a.first() {
                Some(Int(f)) => *f,
                _ => 0,
            };
            let (array, absent, unique) = (flags & 1 != 0, flags & 2 != 0, flags & 4 != 0);
            // json, as json_build_array and json_build_object write it (RETURNING jsonb is a cast of it)
            if array {
                let items: Vec<String> =
                    a[1..].iter().filter(|v| !(absent && **v == Null)).map(super::jsontext::datum).collect();
                return Ok(JsonText(format!("[{}]", items.join(", "))));
            }
            let (mut fields, mut seen) = (vec![], std::collections::HashSet::new());
            for kv in a[1..].chunks(2) {
                if kv[0] == Null {
                    return err("22004", "null value not allowed for object key");
                }
                let k = super::jsontext::key(&kv[0])?;
                let v = kv.get(1).unwrap_or(&Null);
                // (a key left out for its null value still counts; jsonb's check, RETURNING jsonb or a jsonb value
                // without RETURNING, names no key)
                if unique && !seen.insert(k.clone()) {
                    let jsonb = flags & 16 != 0 || (flags & 8 == 0 && a[1..].iter().any(|v| matches!(v, Json(_))));
                    return match jsonb {
                        true => err("22030", "duplicate JSON object key value"),
                        false => err("22030", format!("duplicate JSON object key value: {k}")),
                    };
                }
                if absent && *v == Null {
                    continue;
                }
                fields.push(format!("{k} : {}", super::jsontext::datum(v)));
            }
            return Ok(JsonText(format!("{{{}}}", fields.join(", "))));
        }
        ArrayOf => return Ok(Text(super::array_literal(a))),
        BigintOid => {
            return match &a[0] {
                Null => Ok(Null),
                Int(n @ 0..=0xFFFF_FFFF) => Ok(Int(*n)),
                _ => err(super::RANGE, "OID out of range"),
            };
        }
        Format if a[0] != Null => return Ok(Text(format(&text(&a[0]), &a[1..])?)),
        // (an address is quoted as its cast to text, which has the mask length of an inet too)
        QuoteNullable => {
            return Ok(Text(if a[0] == Null { "NULL".into() } else { quote_literal(&quoted_text(&a[0])?) }));
        }
        // (a null value is the fifth argument's business: the path and target can't be null)
        JsonSetLax if a[2] == Null && !a[0..2].contains(&Null) && !a.get(3).is_some_and(|v| *v == Null) => {
            let treat = a.get(4).map(text).unwrap_or_else(|| "use_json_null".into());
            return match treat.as_str() {
                "use_json_null" => json_set_lax_value(a, Value::Json(serde_json::Value::Null)),
                "delete_key" => {
                    let steps = path_steps(&a[1])?.unwrap_or_default();
                    Ok(Json(super::jsonb::delete_path(&super::as_json(a[0].clone())?, &steps)?))
                }
                "return_target" => Ok(a[0].clone()),
                "raise_exception" => Err(super::SqlError {
                    code: "22004",
                    message: "JSON value must not be null".into(),
                    info: Some(Box::new(super::Violation {
                        detail: Some("Exception was raised because null_value_treatment is \"raise_exception\".".into()),
                        hint: Some(
                            "To avoid, either change the null_value_treatment argument or ensure that an SQL NULL is not passed."
                                .into(),
                        ),
                        ..Default::default()
                    })),
                }),
                _ => err(
                    "22023",
                    "null_value_treatment must be \"delete_key\", \"return_target\", \"use_json_null\", or \"raise_exception\"",
                ),
            };
        }
        ArraySet => return array_set(a),
        ArraySetN => return array_set_n(a),
        // (each its array's text: they must agree on their dimensions, as Postgres says)
        ArrayNest => return super::mdarray::nest(a).map(|x| x.value()),
        // TABLESAMPLE's draw for one row: (percent, seed or null, the row's text, whether a seed was given)
        SampleKeep => {
            if a[0] == Null {
                return err("2202H", "TABLESAMPLE parameter cannot be null");
            }
            let pct = float(&a[0])?;
            if !(0.0..=100.0).contains(&pct) {
                return err("2202H", "sample percentage must be between 0 and 100");
            }
            if a[1] == Null && a[3] == Bool(true) {
                return err("2202G", "TABLESAMPLE REPEATABLE parameter cannot be null");
            }
            let draw = match &a[1] {
                Null => random(),
                seed => {
                    use std::hash::{Hash, Hasher};
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    (text(seed), text(&a[2])).hash(&mut h);
                    (h.finish() >> 11) as f64 / (1u64 << 53) as f64
                }
            };
            return Ok(Bool(draw * 100.0 < pct));
        }
        Random => return Ok(Float(random())),
        // (strict: a null mean or deviation is a null; left out, 0 and 1)
        RandomNormal => {
            if a.contains(&Null) {
                return Ok(Null);
            }
            let arg = |i: usize, d: f64| a.get(i).map_or(Ok(d), float);
            return Ok(Float(random_normal(arg(0, 0.0)?, arg(1, 1.0)?)));
        }
        UuidV1(_) => return Ok(Text(super::new_uuid_v1())),
        Pi => return Ok(Float(std::f64::consts::PI)),
        // (these say what a null does themselves)
        XmlElement | XmlForest | XmlConcat | XmlPi | XmlRoot | XmlComment | XmlParse | XmlSerialize | XmlExistsFn
        | Xpath | XpathExists | XmlWellFormed(_) | IsDocument => return xml_call(f, a),
        _ => {}
    }
    if a.contains(&Null) {
        return Ok(Null);
    }
    if let Some(v) = super::pgcrypto::call(f, a)? {
        return Ok(v);
    }
    if let Some(v) = super::bytes::call(f, a)? {
        return Ok(v);
    }
    if let Some(v) = net_call(f, a)? {
        return Ok(v);
    }
    let s = || text(&a[0]);
    Ok(match f {
        // (named so for their columns only: they are Ceil and Log, as `call` made them)
        Ceiling => return call(Ceil, a),
        Log10 => return call(Log, a),
        Mask(m) => return super::mask::apply(m, a),
        FloatAccum(k) => return super::stats::transition(k, a),
        Float4Text if matches!(a[0], JsonText(_)) => Text(a[0].to_text()),
        Float4Text => Text(super::fmt_float4(float(&a[0])?)),
        BoolArrayText if matches!(a[0], Null) => Null,
        BoolArrayText => Text(super::map_array_text(&text(&a[0]), &mut |x| {
            Ok(x.map(|x| match x.as_str() {
                "t" => "true".to_string(),
                "f" => "false".to_string(),
                _ => x,
            }))
        })?),
        // (held as a JSON number's text, which a row, an array or JSON writes as it is, and a jsonb reads as the
        // number: `1.6777216e+07`)
        Float4Acc => a[0].clone(),
        Float4Round => match &a[0] {
            Float(f) => Float(super::f4_checked(*f)?),
            v => v.clone(),
        },
        Float4Digits if matches!(a[0], JsonText(_)) => a[0].clone(),
        Float4Digits => {
            let x = float(&a[0])?;
            if x.is_finite() { JsonText(super::fmt_float4(x)) } else { Float(x) }
        }
        Float4Numeric => {
            let x = float(&a[0])? as f32;
            if !x.is_finite() {
                return Ok(Numeric(super::decimal::Dec::from_f64(f64::from(x)).unwrap_or(super::decimal::Dec::NAN)));
            }
            match super::decimal::Dec::parse(&super::fmt_g(f64::from(x), 6)) {
                Some(d) => Numeric(d),
                None => return err("22003", "value overflows numeric format"),
            }
        }
        Substring => return call(Substr, a),
        // (all three answered above, whatever their arguments)
        BoolCmp(k) => {
            let (x, y) = (matches!(a[0], Bool(true)), matches!(a[1], Bool(true)));
            Bool(match k {
                0 => x == y,
                1 => x != y,
                2 => x & !y,
                3 => !x & y,
                4 => x >= y,
                6 => x && y,
                7 => x || y,
                _ => x <= y,
            })
        }
        Normalize | IsNormalized => {
            let form = a.get(1).map(text).unwrap_or_else(|| "NFC".into());
            let cow = |x: &str| -> Result<String> {
                use icu_normalizer::{ComposingNormalizer as C, DecomposingNormalizer as D};
                Ok(match form.to_uppercase().as_str() {
                    "NFC" => C::new_nfc().normalize(x).into_owned(),
                    "NFD" => D::new_nfd().normalize(x).into_owned(),
                    "NFKC" => C::new_nfkc().normalize(x).into_owned(),
                    "NFKD" => D::new_nfkd().normalize(x).into_owned(),
                    _ => return err("22023", format!("invalid normalization form: {form}")),
                })
            };
            let x = text(&a[0]);
            let n = cow(&x)?;
            match f {
                Normalize => Text(n),
                _ => Bool(n == x),
            }
        }
        TsLexize => {
            use super::tsearch as ts;
            let dict = text(&a[0]);
            let dict = dict.trim_start_matches("pg_catalog.").trim_start_matches("public.");
            match ts::lexize(dict, &text(&a[1])).map_err(|e| super::SqlError {
                code: "42704",
                message: e,
                info: None,
            })? {
                ts::Lexized::Unknown => Null,
                ts::Lexized::Stop => Text("{}".into()),
                ts::Lexized::Lexemes(l) => Text(super::array_text(l.into_iter().map(Text).collect::<Vec<_>>().iter())),
            }
        }
        XmlElement | XmlForest | XmlConcat | XmlPi | XmlRoot | XmlComment | XmlParse | XmlSerialize | XmlExistsFn
        | Xpath | XpathExists | XmlWellFormed(_) | IsDocument => return xml_call(f, a),
        // misc.c's parse_ident, byte by byte: a bare word downcased (ASCII letters only), not truncated
        ParseIdent => {
            let strict = a.get(1).is_none_or(|v| matches!(v, Bool(true)));
            let src = text(&a[0]);
            let bad = |detail: Option<&str>| -> super::SqlError {
                super::SqlError {
                    code: "22023",
                    message: format!("string is not a valid identifier: \"{src}\""),
                    info: detail.map(|d| Box::new(super::Violation { detail: Some(d.into()), ..Default::default() })),
                }
            };
            let b = src.as_bytes();
            let at = |i: usize| b.get(i).copied().unwrap_or(0);
            // (scanner_isspace)
            let space = |c: u8| matches!(c, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c');
            let start = |c: u8| c == b'_' || c.is_ascii_alphabetic() || c >= 0x80;
            let cont = |c: u8| c.is_ascii_digit() || c == b'$' || start(c);
            let (mut i, mut parts, mut after_dot) = (0usize, vec![], false);
            while space(at(i)) {
                i += 1;
            }
            loop {
                let mut missing = true;
                if at(i) == b'"' {
                    let mut part = vec![];
                    i += 1;
                    loop {
                        match b.get(i) {
                            None => return Err(bad(Some("String has unclosed double quotes."))),
                            Some(b'"') if at(i + 1) == b'"' => {
                                part.push(b'"');
                                i += 2;
                            }
                            Some(b'"') => {
                                i += 1;
                                break;
                            }
                            Some(&c) => {
                                part.push(c);
                                i += 1;
                            }
                        }
                    }
                    if part.is_empty() {
                        return Err(bad(Some("Quoted identifier must not be empty.")));
                    }
                    parts.push(Text(String::from_utf8_lossy(&part).into_owned()));
                    missing = false;
                } else if start(at(i)) {
                    let st = i;
                    i += 1;
                    while cont(at(i)) {
                        i += 1;
                    }
                    parts.push(Text(String::from_utf8_lossy(&b[st..i]).to_ascii_lowercase()));
                    missing = false;
                }
                if missing {
                    return Err(if at(i) == b'.' {
                        bad(Some("No valid identifier before \".\"."))
                    } else if after_dot {
                        bad(Some("No valid identifier after \".\"."))
                    } else {
                        bad(None)
                    });
                }
                while space(at(i)) {
                    i += 1;
                }
                if at(i) == b'.' {
                    after_dot = true;
                    i += 1;
                    while space(at(i)) {
                        i += 1;
                    }
                } else if i >= b.len() {
                    break;
                } else {
                    if strict {
                        return Err(bad(None));
                    }
                    break;
                }
            }
            Text(super::array_text(parts.iter()))
        }
        PointNew => {
            let f = |v: &Value| -> Result<String> { Ok(Value::Float(float(v)?).to_text()) };
            Text(format!("({},{})", f(&a[0])?, f(&a[1])?))
        }
        DateBin => {
            // (a quoted stride, `'15 minutes'`, is an interval)
            let stride = match &a[0] {
                Text(_) => super::coerce(a[0].clone(), Type::Interval)?,
                v => v.clone(),
            };
            let (Interval(i), src, origin) = (&stride, &a[1], &a[2]) else {
                return err(super::WRONG_TYPE, "date_bin takes an interval, a timestamp and a timestamp");
            };
            let (x, o, tz) = match (src, origin) {
                (Timestamp(x), Timestamp(o)) => (*x, *o, false),
                (TimestampTz(x), TimestampTz(o)) => (*x, *o, true),
                (TimestampTz(x), Timestamp(o)) | (Timestamp(x), TimestampTz(o)) => (*x, *o, true),
                _ => return err(super::WRONG_TYPE, "date_bin takes timestamps"),
            };
            let stamp = |r: i64| if tz { TimestampTz(r) } else { Timestamp(r) };
            // (as timestamp_bin checks them, in its order)
            if time::infinite(x) {
                return Ok(stamp(x));
            }
            if time::infinite(o) {
                return err("22008", "origin out of range");
            }
            if i.months != 0 {
                return err("0A000", "timestamps cannot be binned into intervals containing months or years");
            }
            let Some(stride) = i64::from(i.days).checked_mul(time::DAY).and_then(|d| d.checked_add(i.micros)) else {
                return err("22008", "interval out of range");
            };
            if stride <= 0 {
                return err("22023", "stride must be greater than zero");
            }
            let Some(diff) = x.checked_sub(o) else { return err("22008", "interval out of range") };
            let modulo = diff % stride;
            let mut r = o + (diff - modulo);
            // (rounded toward minus infinity, which can leave the timestamps' range)
            if modulo < 0 {
                match r.checked_sub(stride).filter(|r| *r >= time::MIN_TIMESTAMP) {
                    Some(v) => r = v,
                    None => return err("22008", "timestamp out of range"),
                }
            }
            stamp(r)
        }
        TimePrec => {
            let p = u32::try_from(int(&a[1])?).unwrap_or(6);
            match &a[0] {
                Timestamp(t) => Timestamp(super::round_micros(*t, p)),
                TimestampTz(t) => TimestampTz(super::round_micros(*t, p)),
                Time(t) => Time(super::round_micros(*t, p)),
                // (with a range of fields, `::interval day to minute`, the ones after it dropped too)
                Interval(i) if a.len() > 2 => {
                    let range = u32::try_from(int(&a[2])?).unwrap_or(time::INTERVAL_FULL_RANGE);
                    Interval(time::fit_interval(*i, range, Some(p).filter(|p| *p < 6)))
                }
                Interval(i) => Interval(time::Interval { micros: super::round_micros(i.micros, p), ..*i }),
                v => v.clone(),
            }
        }
        DateAdd(subtract) => {
            let t = match super::coerce(a[0].clone(), Type::TimestampTz)? {
                TimestampTz(t) => t,
                _ => return err(super::WRONG_TYPE, "date_add takes a timestamp with time zone"),
            };
            let i = match &a[1] {
                Interval(i) => *i,
                v => time::parse_interval(&v.to_text()).ok_or_else(|| super::SqlError {
                    code: "22007",
                    message: format!("invalid input syntax for type interval: \"{}\"", v.to_text()),
                    info: None,
                })?,
            };
            let i = if subtract {
                match (i.months.checked_neg(), i.days.checked_neg(), i.micros.checked_neg()) {
                    (Some(months), Some(days), Some(micros)) => time::Interval { months, days, micros },
                    _ => return err("22008", "interval out of range"),
                }
            } else {
                i
            };
            if time::infinite(t) {
                return Ok(TimestampTz(t));
            }
            // (the months and days on that zone's calendar: across a daylight saving change a day isn't 24 hours)
            let moved = match a.get(2) {
                Some(z) => {
                    let name = text(z);
                    let zone = match time::zone_named(&name) {
                        Ok(z) => z,
                        Err(message) => {
                            time::abbrev_zone(&name).ok_or(super::SqlError { code: "22023", message, info: None })?
                        }
                    };
                    zone.add_interval(t, i)
                }
                None => time::add_interval_tz(t, i),
            };
            match moved.filter(|t| *t >= time::MIN_TIMESTAMP && !time::infinite(*t)) {
                Some(t) => TimestampTz(t),
                None => return err("22008", "timestamp out of range"),
            }
        }
        ArrayToString | NumNulls | NumNonnulls | HashFn(..) | GeoOp | GeoOpF | GeoOpV | GeoFn(_) | SortKey
        | TimeTzFn(_) | StatReset | ArrayNest | TablespaceLocation | RecordImage | IntArr(_) | Hstore(_) | Ltree(_)
        | JsonFn(_) | XidFn(_) | BitOp(_) | BitFit | BitToInt | BitFn(_) | MoneyOp(_) | MoneyFn(_) | LsnFn(_)
        | BinaryCast | RangeNew(_) | MultiNew(_) | RangeOp | RangeSetOp | RangeBound(_) | RangeFlag(_) | RangeMerge => {
            Null
        }
        // (the network functions: answered by `net_call`)
        NetHost | NetAbbrev | NetBroadcast | NetNetmask | NetHostmask | NetNetwork | NetMasklen | NetFamily
        | NetSetMasklen | NetSameFamily | NetMerge | NetSet7bit | NetInet | NetCidr | NetMacaddr | NetMacaddr8 => Null,
        // (bytea's, and the text ones that need no bytea: answered by `bytes::call`)
        Encode | Decode | Sha224 | Sha256 | Sha384 | Sha512 | GetByte | SetByte | BitLength | ConvertTo
        | ConvertFrom => Null,
        // (pgcrypto's: answered by `pgcrypto::call`)
        Digest | Hmac | Crypt | GenSalt | GenRandomBytes | Encrypt | Decrypt | EncryptIv | DecryptIv
        | PgpSymEncrypt | PgpSymEncryptBytea | PgpSymDecrypt | PgpSymDecryptBytea | PgpKeyId | Armor | Dearmor => Null,
        Overlay => {
            let (cs, by) = (chars(&s())?, chars(&text(&a[1]))?);
            let from = int(&a[2])?;
            let len = a.get(3).map(int).transpose()?.unwrap_or(by.len() as i64);
            let head = ((from - 1).max(0) as usize).min(cs.len());
            let tail = (from.saturating_add(len).max(1) - 1).clamp(0, cs.len() as i64) as usize;
            Text(cs[..head].iter().chain(&by).chain(&cs[tail.max(head)..]).collect())
        }
        // substring(s from pattern): what a POSIX regular expression matches (its first
        // parenthesized part, if it has one), and with an escape (`for e`, or `similar p escape e`)
        // what a SQL one matches between its escape-double-quotes
        Substr if a.len() >= 2 && matches!(a[1], Value::Text(_)) => {
            let pattern = match a.get(2) {
                None => text(&a[1]),
                Some(e) => match text(e) {
                    e if e.chars().count() > 1 => return Err(bad_escape()),
                    e => similar_escape(&text(&a[1]), e.chars().next())?,
                },
            };
            let re = super::compiled(&pattern, "")?;
            match super::regex::first_match(&re, &s())? {
                Some(groups) => groups.into_iter().next().flatten().map_or(Null, Text),
                None => Null,
            }
        }
        Substr => {
            let cs = chars(&s())?;
            let start = int(&a[1])?;
            let end = match a.get(2) {
                Some(n) => {
                    let n = int(n)?;
                    if n < 0 {
                        return err("22011", "negative substring length not allowed");
                    }
                    start.saturating_add(n)
                }
                None => i64::MAX,
            };
            let (from, to) = ((start.max(1) - 1) as usize, (end.max(1) - 1).min(cs.len() as i64) as usize);
            Text(if from < to { cs[from..to].iter().collect() } else { String::new() })
        }
        Strpos | Position => {
            let (hay, needle) = (s(), text(&a[1]));
            Int(hay.find(&needle).map_or(0, |b| hay[..b].chars().count() as i64 + 1))
        }
        Replace => {
            let from = text(&a[1]);
            if from.is_empty() {
                Text(s())
            } else {
                let (s, to) = (s(), text(&a[2]));
                let n = s.matches(from.as_str()).count();
                fits(n.checked_mul(to.len()).and_then(|b| b.checked_add(s.len() - n * from.len())))?;
                Text(s.replace(&from, &to))
            }
        }
        Btrim | Ltrim | Rtrim => {
            let set = a.get(1).map_or(" ".to_string(), text);
            Text(trim(&s(), &set, f != Rtrim, f != Ltrim))
        }
        // (a cast to varchar of no length, marked by its third argument: as it is)
        Left if a.len() == 3 && int(&a[1])? == i64::from(u32::MAX) => Text(s()),
        Left | Right => {
            let (cs, n) = (chars(&s())?, int(&a[1])?);
            let len = cs.len() as i64;
            let take = if n >= 0 { n.min(len) } else { (len + n).max(0) } as usize;
            let cut: String =
                if f == Left { cs[..take].iter().collect() } else { cs[cs.len() - take..].iter().collect() };
            // (a cast to char(n) is a third argument true: its trailing blanks count for nothing, as a column's don't)
            let fixed = a.len() == 3 && f == Left && matches!(a[2], Bool(true));
            Text(if fixed { cut.trim_end_matches(' ').to_string() } else { cut })
        }
        Lpad | Rpad => {
            let fill = a.get(2).map_or(" ".to_string(), text);
            Text(pad(&s(), int(&a[1])?, &fill, f == Lpad)?)
        }
        Repeat => {
            let (s, n) = (s(), usize::try_from(int(&a[1])?.max(0)).unwrap_or(usize::MAX));
            fits(s.len().checked_mul(n))?;
            Text(s.repeat(n))
        }
        Reverse => Text(s().chars().rev().collect()),
        Translate => {
            // each character of the second text is replaced by the third's at its place, or dropped
            // if the third is shorter (a character named twice takes its first place)
            let (from, to) = (chars(&text(&a[1]))?, chars(&text(&a[2]))?);
            Text(
                s().chars()
                    .filter_map(|c| match from.iter().position(|f| *f == c) {
                        Some(i) => to.get(i).copied(),
                        None => Some(c),
                    })
                    .collect(),
            )
        }
        ToNumber => match super::tonumber::to_number_opt(&text(&a[0]), &text(&a[1])) {
            Ok(Some(t)) => match super::decimal::Dec::parse(&t) {
                Some(d) => Numeric(d),
                None => match t.parse::<f64>() {
                    Ok(f) if f.is_finite() => Float(f),
                    _ => return err(super::RANGE, "value overflows numeric format"),
                },
            },
            Ok(None) => Null,
            Err((code, message)) => return err(code, message),
        },
        Char => Text(one_byte(&s())),
        Unistr => Text(unescape(&s(), '\\', true).map_err(|(code, message)| {
            let hint = (message == "invalid Unicode escape")
                .then(|| "Unicode escapes must be \\XXXX, \\+XXXXXX, \\uXXXX, or \\UXXXXXXXX.".to_string());
            super::SqlError {
                code,
                message,
                info: hint.map(|h| Box::new(super::Violation { hint: Some(h), ..super::Violation::default() })),
            }
        })?),
        Similarity => real(pg(textext::similarity(&s(), &text(&a[1])))?),
        WordSimilarity => real(pg(textext::word_similarity(&s(), &text(&a[1])))?),
        StrictWordSimilarity => real(pg(textext::strict_word_similarity(&s(), &text(&a[1])))?),
        ShowTrgm => {
            Text(super::array_literal(&pg(textext::show_trgm(&s()))?.into_iter().map(Text).collect::<Vec<_>>()))
        }
        TrgmTest | TrgmDist => trgm_op(&text(&a[0]), &text(&a[1]), &text(&a[2]))?,
        Levenshtein | LevenshteinLessEqual => {
            let (x, y) = (s(), text(&a[1]));
            let n = |i: usize| int(&a[i]).map(|n| n as i32);
            let d = match (f, a.len()) {
                (Levenshtein, 2) => textext::levenshtein(&x, &y),
                (Levenshtein, _) => textext::levenshtein_with_costs(&x, &y, n(2)?, n(3)?, n(4)?),
                (_, 3) => textext::levenshtein_less_equal(&x, &y, n(2)?),
                _ => textext::levenshtein_less_equal_with_costs(&x, &y, n(2)?, n(3)?, n(4)?, n(5)?),
            };
            Int(i64::from(pg(d)?))
        }
        Soundex => Text(textext::soundex(&s())),
        Difference => Int(i64::from(textext::difference(&s(), &text(&a[1])))),
        Metaphone => Text(textext::metaphone(&s(), int(&a[1])? as i32).or_else(|(code, m)| err(code, m))?),
        Dmetaphone => Text(textext::dmetaphone(&s())),
        DmetaphoneAlt => Text(textext::dmetaphone_alt(&s())),
        DaitchMokotoff => match textext::daitch_mokotoff(&s()) {
            Some(codes) => Text(super::array_literal(&codes.into_iter().map(Text).collect::<Vec<_>>())),
            None => Null,
        },
        Unaccent => {
            // unaccent(dictionary, text): the only dictionary there is is `unaccent`
            let dict = text(&a[0]);
            let dict = dict.rsplit('.').next().unwrap_or("").trim_matches('"');
            if a.len() == 2 && !dict.eq_ignore_ascii_case("unaccent") {
                return err("42704", format!("text search dictionary \"{}\" does not exist", text(&a[0])));
            }
            Text(pg(textext::unaccent(&text(&a[a.len() - 1])))?)
        }
        SplitPart => {
            let (s, delim, n) = (s(), text(&a[1]), int(&a[2])?);
            if n == 0 {
                return err("22023", "field position must not be zero");
            }
            let parts: Vec<&str> = if delim.is_empty() { vec![&s] } else { s.split(delim.as_str()).collect() };
            let i = if n > 0 { n - 1 } else { parts.len() as i64 + n };
            Text(usize::try_from(i).ok().and_then(|i| parts.get(i)).map_or(String::new(), |p| p.to_string()))
        }
        Initcap => {
            let mut out = String::new();
            let mut word = false;
            for c in s().chars() {
                if out.len() > MAX_BYTES {
                    return err("54000", "requested length too large"); // a case changed can grow
                }
                if c.is_alphanumeric() {
                    out.push(if word { lower_char(c) } else { upper_char(c) });
                    word = true;
                } else {
                    out.push(c);
                    word = false;
                }
            }
            Text(out)
        }
        OctetLength => Int(s().len() as i64),
        Ascii => Int(s().chars().next().map_or(0, |c| c as i64)),
        Chr => {
            let n = int(&a[0])?;
            let bad = |m: String| err("54000", m);
            if i32::try_from(n).is_err() {
                return err("42883", "function chr(bigint) does not exist");
            }
            if n < 0 {
                return err("22023", "character number must be positive");
            }
            if n == 0 {
                return bad("null character not permitted".into());
            }
            match u32::try_from(n).ok() {
                None | Some(0x110000..) => return bad(format!("requested character too large for encoding: {n}")),
                Some(n) => match char::from_u32(n) {
                    Some(c) => Text(c.to_string()),
                    None => return bad(format!("requested character not valid for encoding: {n}")),
                },
            }
        }
        StartsWith => Bool(s().starts_with(&text(&a[1]))),
        JsonObjectOf => {
            // (an array of one dimension; a JSON array is one)
            let shaped = |v: &Value| match v {
                Text(t) => super::array_shape(t),
                v => super::array_values(v.clone()).map(|x| {
                    let x = x.unwrap_or_default();
                    (if x.is_empty() { vec![] } else { vec![x.len()] }, x)
                }),
            };
            let (keys, vals): (Vec<Value>, Vec<Value>) = match a.get(1) {
                Some(v) => {
                    let ((kd, keys), (vd, vals)) = (shaped(&a[0])?, shaped(v)?);
                    if kd.len() > 1 || vd.len() > 1 {
                        return err("2202E", "wrong number of array subscripts");
                    }
                    (keys, vals)
                }
                None => {
                    let (dims, all) = shaped(&a[0])?;
                    match dims.len() {
                        0 | 1 if all.len() % 2 == 1 => {
                            return err("2202E", "array must have even number of elements");
                        }
                        0 | 1 => {}
                        2 if dims[1] != 2 => return err("2202E", "array must have two columns"),
                        2 => {}
                        _ => return err("2202E", "wrong number of array subscripts"),
                    }
                    (all.iter().step_by(2).cloned().collect(), all.iter().skip(1).step_by(2).cloned().collect())
                }
            };
            if keys.len() != vals.len() {
                return err("2202E", "mismatched array dimensions");
            }
            let mut o = serde_json::Map::new();
            for (k, v) in keys.iter().zip(&vals) {
                if *k == Null {
                    return err("22004", "null value not allowed for object key");
                }
                o.insert(
                    k.to_text(),
                    if *v == Null { serde_json::Value::Null } else { serde_json::Value::String(v.to_text()) },
                );
            }
            Json(super::jsonb::shallow(serde_json::Value::Object(o))?)
        }
        IsJson => {
            let spec = text(&a[1]);
            // (" unique" after the kind: WITH UNIQUE KEYS)
            let (kind, unique) = match spec.strip_suffix(" unique") {
                Some(k) => (k.to_string(), true),
                None => (spec.clone(), false),
            };
            // the text it is: a bytea's bytes read as UTF-8, as Postgres reads them
            let doc: Option<String> = match &a[0] {
                Json(_) => None,
                Bytea(b) => Some(utf8_text(b)?),
                v => Some(text(v)),
            };
            let parsed: Option<serde_json::Value> = match (&a[0], &doc) {
                (Json(j), _) => Some(j.clone()),
                (_, Some(d)) => serde_json::from_str(d).ok(),
                (_, None) => None,
            };
            // (a jsonb has each key once)
            let keys_ok = !unique || doc.as_deref().is_none_or(super::jsontext::unique_keys);
            Bool(match (parsed, kind.as_str()) {
                (None, _) => false,
                (Some(_), _) if !keys_ok => false,
                (Some(j), "object") => j.is_object(),
                (Some(j), "array") => j.is_array(),
                (Some(j), "scalar") => !j.is_object() && !j.is_array(),
                (Some(_), _) => true,
            })
        }
        Ceil | Floor | Trunc | Sign => match super::num(a[0].clone())? {
            Int(n) if a.len() == 1 => Float(if f == Sign { n.signum() as f64 } else { n as f64 }),
            Numeric(d) if d.is_special() => Numeric(match f {
                Sign if !d.is_nan() => super::decimal::Dec::int(i64::from(d.signum())),
                _ => d,
            }),
            // numeric_ceil, numeric_floor, numeric_sign and numeric_trunc (places may be negative: left of the point)
            Numeric(d) => {
                let places = a.get(1).map(int).transpose()?.unwrap_or(0);
                let places = places.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
                let range = |e: &str| super::SqlError { code: super::RANGE, message: e.into(), info: None };
                Numeric(match f {
                    Ceil => d.ceil().map_err(range)?,
                    Floor => d.floor().map_err(range)?,
                    Sign => super::decimal::Dec::int(i64::from(d.signum())),
                    _ => d.trunc_to(places).map_err(range)?,
                })
            }
            v => {
                let x = float(&v)?;
                let scale = match a.get(1) {
                    Some(p) => 10f64.powi(int(p)?.clamp(-20, 20) as i32),
                    None => 1.0,
                };
                Float(match f {
                    Ceil => x.ceil(),
                    Floor => x.floor(),
                    Sign => x.signum() * f64::from(x != 0.0),
                    _ => (x * scale).trunc() / scale,
                })
            }
        },
        Sqrt if matches!(a[0], Numeric(_)) => {
            let Numeric(d) = &a[0] else { unreachable!("matched a numeric") };
            Numeric(d.sqrt().map_err(|e| super::SqlError {
                code: super::decimal::sqlstate(e),
                message: e.into(),
                info: None,
            })?)
        }
        Sqrt => {
            let x = float(&a[0])?;
            if x < 0.0 {
                return err("2201F", "cannot take square root of a negative number");
            }
            Float(x.sqrt())
        }
        Cbrt => Float(float(&a[0])?.cbrt()),
        Erf => Float(cmath::erf(float(&a[0])?)),
        Erfc => Float(cmath::erfc(float(&a[0])?)),
        // numeric's own ln, log and exp: exact to Postgres's result scale; log(b, x) is numeric's alone
        Exp | Ln | Log
            if !a.iter().any(|v| matches!(v, Float(_)))
                && (a.iter().any(|v| matches!(v, Numeric(_))) || (f == Log && a.len() == 2)) =>
        {
            use super::decimal::Dec;
            let r = match (f, a.len()) {
                (Exp, _) => exact(&a[0])?.exp(),
                (Ln, _) => exact(&a[0])?.ln(),
                (_, 1) => Dec::log(Dec::int(10), exact(&a[0])?),
                _ => Dec::log(exact(&a[0])?, exact(&a[1])?),
            };
            Numeric(r.map_err(|e| super::SqlError {
                code: super::decimal::sqlstate(e),
                message: e.into(),
                info: None,
            })?)
        }
        // (dexp: an infinity gives itself or 0; past the range, Postgres's overflow and underflow)
        Exp => {
            let x = float(&a[0])?;
            if x.is_nan() || x.is_infinite() {
                return Ok(Float(if x == f64::NEG_INFINITY { 0.0 } else { x }));
            }
            let r = x.exp();
            if r.is_infinite() {
                return err(super::RANGE, "value out of range: overflow");
            }
            if r == 0.0 {
                return err(super::RANGE, "value out of range: underflow");
            }
            Float(r)
        }
        Ln | Log => {
            let (base, x) = match (f, a.len()) {
                (Ln, _) => (std::f64::consts::E, float(&a[0])?),
                (_, 1) => (10.0, float(&a[0])?),
                _ => (float(&a[0])?, float(&a[1])?),
            };
            if x <= 0.0 {
                return err(
                    "2201E",
                    if x == 0.0 {
                        "cannot take logarithm of zero"
                    } else {
                        "cannot take logarithm of a negative number"
                    },
                );
            }
            Float(if base == 10.0 { x.log10() } else { x.ln() / base.ln() })
        }
        Power => {
            if let Some(r) = super::numeric_power_of(&a[0], &a[1]) {
                return r;
            }
            return super::float_power(float(&a[0])?, float(&a[1])?);
        }
        IsFinite => Bool(match &a[0] {
            Date(d) => !time::infinite_date(*d),
            Timestamp(t) | TimestampTz(t) => !time::infinite(*t),
            Interval(_) => true,
            v => return err(super::NO_FUNCTION, format!("function isfinite({}) does not exist", v.kind())),
        }),
        JustifyHours | JustifyDays | JustifyInterval => {
            let Interval(i) = a[0] else { return err(super::WRONG_TYPE, "expected an interval") };
            let out = match f {
                JustifyHours => time::justify_hours(i),
                JustifyDays => time::justify_days(i),
                _ => time::justify_interval(i),
            };
            Interval(out.map_err(|m| super::SqlError { code: "22015", message: m.into(), info: None })?)
        }
        Degrees => Float(float(&a[0])? / RADIANS_PER_DEGREE),
        Radians => Float(float(&a[0])? * RADIANS_PER_DEGREE),
        Sin | Cos | Tan | Cot => {
            let x = float(&a[0])?;
            if x.is_infinite() {
                return err(super::RANGE, "input is out of range");
            }
            Float(match f {
                Sin => x.sin(),
                Cos => x.cos(),
                Tan => x.tan(),
                _ => 1.0 / x.tan(),
            })
        }
        Asin | Acos => {
            let x = float(&a[0])?;
            if !(-1.0..=1.0).contains(&x) && !x.is_nan() {
                return err(super::RANGE, "input is out of range");
            }
            Float(if f == Asin { x.asin() } else { x.acos() })
        }
        Atan => Float(float(&a[0])?.atan()),
        Atan2 => Float(float(&a[0])?.atan2(float(&a[1])?)),
        Sind | Cosd | Tand | Cotd | Asind | Acosd | Atand | Atan2d => {
            let x = float(&a[0])?;
            if f == Atan2d {
                let c = degree_constants();
                return Ok(Float((x.atan2(float(&a[1])?) / c.atan_1_0) * 45.0));
            }
            let r = degree_fn(f, x)?;
            // (tan and cot have no negative zero)
            Float(if r == 0.0 && matches!(f, Tand | Cotd) { 0.0 } else { r })
        }
        Asinh => Float(float(&a[0])?.asinh()),
        Acosh => {
            let x = float(&a[0])?;
            if x < 1.0 {
                return err(super::RANGE, "input is out of range");
            }
            Float(x.acosh())
        }
        Atanh => {
            let x = float(&a[0])?;
            if !(-1.0..=1.0).contains(&x) && !x.is_nan() {
                return err(super::RANGE, "input is out of range");
            }
            Float(x.atanh())
        }
        Scale | MinScale | TrimScale => {
            let d = exact(&a[0])?;
            if d.is_special() {
                return Ok(if f == TrimScale { Numeric(d) } else { Null });
            }
            // digits after the point with the trailing zeros off
            match f {
                Scale => Int(i64::from(d.scale())),
                MinScale => Int(i64::from(d.min_scale())),
                _ => Numeric(d.normal()),
            }
        }
        Sinh | Cosh | Tanh => {
            let x = float(&a[0])?;
            Float(match f {
                Sinh => x.sinh(),
                Cosh => x.cosh(),
                _ => x.tanh(),
            })
        }
        // numeric_div_trunc: a / b cut toward zero to a whole number
        Div => {
            let (x, y) = (exact(&a[0])?, exact(&a[1])?);
            Numeric(super::decimal::Dec::div_trunc(x, y).map_err(|e| super::SqlError {
                code: super::decimal::sqlstate(e),
                message: e.into(),
                info: None,
            })?)
        }
        NumericInc => {
            let x = exact(&a[0])?;
            Numeric(x.inc().map_err(|e| super::SqlError { code: super::RANGE, message: e.into(), info: None })?)
        }
        // gcd and lcm of numerics: of the values at the larger scale, at that scale; NaN with NaN or an infinity
        Gcd | Lcm if matches!(a[0], Numeric(_)) || matches!(a[1], Numeric(_)) => {
            use super::decimal::Dec;
            let (x, y) = (exact(&a[0])?, exact(&a[1])?);
            let r = if f == Gcd { Dec::gcd(x, y) } else { Dec::lcm(x, y) };
            Numeric(r.map_err(|e| super::SqlError {
                code: super::decimal::sqlstate(e),
                message: e.into(),
                info: None,
            })?)
        }
        Gcd | Lcm => {
            let (x, y) = (int(&a[0])? as i128, int(&a[1])? as i128);
            let gcd = {
                let (mut p, mut q) = (x.abs(), y.abs());
                while q != 0 {
                    (p, q) = (q, p % q);
                }
                p
            };
            let r = match f {
                Gcd => gcd,
                _ if gcd == 0 => 0,
                _ => (x / gcd * y).abs(),
            };
            // int4 arguments give an int4, anything wider an int8
            let wide = [x, y].iter().any(|v| i32::try_from(*v).is_err());
            match i64::try_from(r) {
                Ok(r) if wide || i32::try_from(r).is_ok() => Int(r),
                _ => return err(super::RANGE, if wide { "bigint out of range" } else { "integer out of range" }),
            }
        }
        // numeric_fac (both of its errors are 22003)
        Factorial => {
            let n = int(&a[0])?;
            Numeric(super::decimal::Dec::factorial(n).map_err(|e| super::SqlError {
                code: super::RANGE,
                message: e.into(),
                info: None,
            })?)
        }
        // width_bucket(operand, thresholds): the polymorphic array form
        WidthBucket if a.len() == 2 => width_bucket_array(a)?,
        WidthBucket if a.len() == 3 => {
            return err("42883", "function width_bucket() takes an operand and a thresholds array, or four arguments");
        }
        WidthBucket => width_bucket(a)?,
        // float4send, float8send: the IEEE bits, big-endian, as the binary protocol sends them
        FloatSend(four) => {
            let x = float(&a[0])?;
            Bytea(if four { (x as f32).to_bits().to_be_bytes().to_vec() } else { x.to_bits().to_be_bytes().to_vec() })
        }
        ToHex => {
            let n = int(&a[0])?;
            // two's complement, as wide as the number needs: int4 or int8
            Text(if i32::try_from(n).is_ok() { format!("{:x}", n as i32) } else { format!("{n:x}") })
        }
        SetSeed => setseed(float(&a[0])?)?,
        Mod => match (super::num(a[0].clone())?, super::num(a[1].clone())?) {
            // (a numeric's own rules: NaN and the infinities have no division by zero, see `Dec::rem`)
            (x @ Numeric(_), y @ (Numeric(_) | Int(_))) | (x @ Int(_), y @ Numeric(_)) => {
                return super::binop(super::Op::Mod, x, y);
            }
            (_, Int(0)) => return err("22012", "division by zero"),
            (Int(x), Int(y)) => Int(x.wrapping_rem(y)),
            (x, y) => Float(float(&x)? % float(&y)?),
        },
        // (years, months, weeks, days, hours, mins, secs): each 0 unless given
        Grouping => {
            return err("42803", "arguments to GROUPING must be grouping expressions of the associated query level");
        }
        MakeInterval => {
            let n = |i: usize| a.get(i).map_or(Ok(0), int);
            let (years, months, weeks, days, hours, mins) = (n(0)?, n(1)?, n(2)?, n(3)?, n(4)?, n(5)?);
            let secs = a.get(6).map_or(Ok(0.0), float)?;
            if !secs.is_finite() {
                return err("22008", "interval out of range");
            }
            let months = years.checked_mul(12).and_then(|y| y.checked_add(months)).and_then(|m| i32::try_from(m).ok());
            let days = weeks.checked_mul(7).and_then(|w| w.checked_add(days)).and_then(|d| i32::try_from(d).ok());
            let micros = hours
                .checked_mul(3600)
                .and_then(|h| mins.checked_mul(60).and_then(|m| h.checked_add(m)))
                .and_then(|s| s.checked_mul(1_000_000))
                .and_then(|us| us.checked_add((secs * 1e6).round() as i64));
            match (months, days, micros) {
                (Some(months), Some(days), Some(micros)) => Interval(time::Interval { months, days, micros }),
                _ => return err("22008", "interval out of range"),
            }
        }
        ArrayAppend | ArrayPrepend | ArrayCat | ArrayRemove | ArrayReplace | ArrayPosition | ArrayPositions
        | ArrayFill => unreachable!("handled before the null rule"),
        // (null for an empty array)
        ArrayDims | ArrayNdims => match super::mdarray::read(&a[0])? {
            Some(x) if !x.dims.is_empty() => match f {
                ArrayNdims => Int(x.dims.len() as i64),
                _ => Text(x.dims_text()),
            },
            _ => Null,
        },
        // the last n of the first dimension taken off, as a slice is (from 1)
        TrimArray => {
            let x = super::mdarray::read(&a[0])?.unwrap_or_else(super::mdarray::Arr::empty);
            let len = x.dims.first().map_or(0, |d| d.len as i64);
            let n = int(&a[1])?;
            if !(0..=len).contains(&n) {
                return err("2202E", format!("number of elements to trim must be between 0 and {len}"));
            }
            match x.dims.first() {
                Some(d) => x.slice(&[(i64::MIN, d.lo + (len - n) - 1)]).value(),
                None => Text("{}".into()),
            }
        }
        // (array_shuffle keeps the dimensions; array_sample's first starts at 1, as array_shuffle_n makes it)
        ArrayShuffle | ArraySample => {
            let x = super::mdarray::read(&a[0])?.unwrap_or_else(super::mdarray::Arr::empty);
            let len = x.dims.first().map_or(0, |d| d.len);
            let n = if f == ArraySample {
                let n = int(&a[1])?;
                if n < 0 || n > len as i64 {
                    return err("22023", format!("sample size must be between 0 and {len}"));
                }
                n as usize
            } else {
                len
            };
            if len == 0 {
                return Ok(Text("{}".into()));
            }
            // each item of the first dimension is this many elements
            let per = x.elems.len() / len;
            let mut order: Vec<usize> = (0..len).collect();
            for i in 0..n {
                let j = i + ((random() * (len - i) as f64) as usize).min(len - i - 1);
                order.swap(i, j);
            }
            let elems = order[..n].iter().flat_map(|&k| x.elems[k * per..(k + 1) * per].iter().cloned()).collect();
            if n == 0 {
                return Ok(Text("{}".into()));
            }
            let mut dims = x.dims.clone();
            dims[0].len = n;
            if f == ArraySample {
                dims[0].lo = 1;
            }
            super::mdarray::Arr { dims, elems }.value()
        }
        // a[lo:hi]: the elements from lo to hi (1-based, as far as the array goes), as an array of
        // its own that starts at 1; nothing to take makes an empty one
        // a[i] = v, a[lo:hi] = v (in UPDATE ... SET): the array with its element or slice replaced; a
        // place past either end grows the array with nulls
        ArraySet | ArraySetN | SampleKeep => unreachable!("handled above"),
        // (and a[lo:hi][lo:hi]...: a range for each of the first dimensions, the rest whole; see `mdarray::Arr::slice`)
        ArraySlice => {
            let x = super::mdarray::read(&a[0])?.unwrap_or_else(super::mdarray::Arr::empty);
            let mut ranges = vec![];
            for p in a[1..].chunks(2) {
                ranges.push((int(&p[0])?, int(p.get(1).unwrap_or(&p[0]))?));
            }
            x.slice(&ranges).value()
        }
        MakeDate => {
            let (y, m, d) = (int(&a[0])?, int(&a[1])?, int(&a[2])?);
            match time::make_date(y, m, d) {
                Some(days) => Date(days),
                None => return err("22008", format!("date field value out of range: {y}-{m:02}-{d:02}")),
            }
        }
        MakeTime | MakeTimestamp | MakeTimestampTz => {
            let at = if f == MakeTime { 0 } else { 3 };
            let (h, mi, sec) = (int(&a[at])?, int(&a[at + 1])?, float(&a[at + 2])?);
            let Some(t) = time::make_time(h, mi, sec) else {
                return err("22008", format!("time field value out of range: {h}:{mi:02}:{sec:02}"));
            };
            if f == MakeTime {
                Time(t)
            } else {
                let (y, m, d) = (int(&a[0])?, int(&a[1])?, int(&a[2])?);
                let Some(days) = time::make_date(y, m, d) else {
                    return err("22008", format!("date field value out of range: {y}-{m:02}-{d:02}"));
                };
                let us = i64::from(days) * time::DAY + t;
                if f == MakeTimestamp {
                    Timestamp(us)
                } else {
                    // read in the zone it names, else in the session's
                    let off = match a.get(6) {
                        // (an offset in the ISO way, east positive, not as SET TIME ZONE's POSIX: parse_sane_timezone)
                        Some(z) => match time::sane_numeric_zone(&text(z)) {
                            Err(time::SaneZoneErr::NoSign) => {
                                return Err(super::SqlError {
                                    code: "22023",
                                    message: format!(
                                        "invalid input syntax for type numeric time zone: \"{}\"",
                                        text(z)
                                    ),
                                    info: Some(Box::new(super::Violation {
                                        hint: Some(
                                            "Numeric time zones must have \"-\" or \"+\" as first character.".into(),
                                        ),
                                        ..super::Violation::default()
                                    })),
                                });
                            }
                            Err(time::SaneZoneErr::OutOfRange) => {
                                return err("22023", format!("numeric time zone \"{}\" out of range", text(z)));
                            }
                            Ok(Some(secs)) => i64::from(secs) * 1_000_000,
                            Ok(None) => {
                                let zone = match time::zone_named(&text(z)) {
                                    Ok(z) => z,
                                    Err(message) => time::abbrev_zone(&text(z)).ok_or(super::SqlError {
                                        code: "22023",
                                        message,
                                        info: None,
                                    })?,
                                };
                                zone.offset_for_local(us)
                            }
                        },
                        None => time::offset_for_local(us),
                    };
                    TimestampTz(us - off)
                }
            }
        }
        ToDate | ToTimestamp if a.len() == 2 => {
            // read out of text by a template
            let p =
                super::datefmt::parse(&text(&a[0]), &text(&a[1])).map_err(|e| date_format_error(e.code, e.message))?;
            if f == ToDate {
                Date(p.days)
            } else {
                let wall = i64::from(p.days).checked_mul(time::DAY).and_then(|d| d.checked_add(p.micros));
                let us = wall.and_then(|w| {
                    let off = match p.tz {
                        Some(secs) => secs * 1_000_000,
                        None => time::offset_for_local(w),
                    };
                    w.checked_sub(off)
                });
                match us {
                    // (FFn's digits are the timestamp's precision: it is rounded to them)
                    Some(us) if !time::infinite(us) => TimestampTz(match p.fprec {
                        Some(n) => super::round_micros(us, n),
                        None => us,
                    }),
                    _ => return err("22008", "timestamp out of range"),
                }
            }
        }
        ToDate => unreachable!("takes two arguments"),
        ToTimestamp => {
            // (as float8_timestamptz: infinities are infinite timestamps, NaN none)
            let secs = float(&a[0])?;
            if secs.is_nan() {
                return err("22008", "timestamp cannot be NaN");
            }
            if secs.is_infinite() {
                return Ok(TimestampTz(if secs < 0.0 { time::TS_NEG_INF } else { time::TS_INF }));
            }
            let us = (secs * 1e6).round_ties_even();
            if us < time::MIN_TIMESTAMP as f64 || us >= time::TS_INF as f64 {
                return err("22008", format!("timestamp out of range: \"{}\"", float_g(secs)));
            }
            TimestampTz(us as i64)
        }
        ToJson => Json(a[0].to_json()),
        ArrayJson => super::array_json(a[0].clone(), a[1].clone())?,
        JsonTypeof => Text(super::jsonb::typeof_(&super::as_json(a[0].clone())?).into()),
        JsonArrayLength => match super::as_json(a[0].clone())? {
            serde_json::Value::Array(x) => Int(x.len() as i64),
            serde_json::Value::Object(_) => return err("22023", "cannot get array length of a non-array"),
            _ => return err("22023", "cannot get array length of a scalar"),
        },
        JsonPath(kind, tz) => {
            let Some((path, target, vars, silent)) = path_args(a)? else { return Ok(Null) };
            let run = |e: jsonpath::PathError| Err::<Value, _>(super::path_error(e));
            let out = match kind {
                PathFn::Exists => {
                    jsonpath::exists_value(&path, &target, &vars, silent, tz).map(|b| b.map_or(Null, Bool))
                }
                PathFn::Match => {
                    jsonpath::predicate_value(&path, &target, &vars, silent, tz).map(|b| b.map_or(Null, Bool))
                }
                PathFn::QueryArray | PathFn::QueryFirst => jsonpath::query_value(&path, &target, &vars, silent, tz)
                    .and_then(|items| {
                        let items =
                            items.iter().map(jsonpath::Value::to_json).collect::<std::result::Result<Vec<_>, _>>()?;
                        Ok(match kind {
                            PathFn::QueryFirst => items.into_iter().next().map_or(Null, Json),
                            _ => Json(serde_json::Value::Array(items)),
                        })
                    }),
            };
            match out {
                Ok(v) => v,
                Err(e) => return run(e),
            }
        }
        JsonExtractPath | JsonExtractPathText => {
            let steps: Vec<String> = a[1..].iter().map(Value::to_text).collect();
            let j = super::as_json(a[0].clone())?;
            match (super::jsonb::path(&j, &steps), f) {
                (None, _) => Null,
                (Some(v), JsonExtractPathText) => super::jsonb::as_text(v).map_or(Null, Text),
                (Some(v), _) => Json(v.clone()),
            }
        }
        JsonSet => {
            let Some(steps) = path_steps(&a[1])? else { return Ok(Null) };
            let create = match a.get(3) {
                Some(c) => super::truth(c)?.unwrap_or(true),
                None => true,
            };
            Json(super::jsonb::shallow(super::jsonb::set(
                &super::as_json(a[0].clone())?,
                &steps,
                &super::as_json(a[2].clone())?,
                create,
            )?)?)
        }
        JsonSetLax => json_set_lax_value(a, a[2].clone())?,
        JsonInsert => {
            let Some(steps) = path_steps(&a[1])? else { return Ok(Null) };
            let after = match a.get(3) {
                Some(c) => super::truth(c)?.unwrap_or(false),
                None => false,
            };
            Json(super::jsonb::shallow(super::jsonb::insert(
                &super::as_json(a[0].clone())?,
                &steps,
                &super::as_json(a[2].clone())?,
                after,
            )?)?)
        }
        JsonPretty => Text(super::jsonb::pretty(&super::as_json(a[0].clone())?)),
        JsonDelete => {
            let j = super::as_json(a[0].clone())?;
            Json(match &a[1] {
                Int(n) => super::jsonb::delete_index(&j, *n)?,
                k => super::jsonb::delete_key(&j, &k.to_text())?,
            })
        }
        JsonDeleteKeys => {
            let mut j = super::as_json(a[0].clone())?;
            for k in super::array_values(a[1].clone())?.unwrap_or_default() {
                if k != Null {
                    j = super::jsonb::delete_key(&j, &k.to_text())?;
                }
            }
            Json(j)
        }
        JsonDeletePath => {
            let Some(steps) = path_steps(&a[1])? else { return Ok(Null) };
            Json(super::jsonb::delete_path(&super::as_json(a[0].clone())?, &steps)?)
        }
        JsonStripNulls => Json(super::jsonb::strip_nulls(&super::as_json(a[0].clone())?)),
        Age => {
            let at = |v: &Value| match super::coerce(v.clone(), Type::Timestamp)? {
                Timestamp(t) => Ok(t),
                _ => err(super::WRONG_TYPE, "age() takes timestamps"),
            };
            let (later, earlier) = match a.len() {
                1 => (time::today() as i64 * time::DAY, at(&a[0])?), // since midnight today, in the session's zone
                _ => (at(&a[0])?, at(&a[1])?),
            };
            Interval(time::age(later, earlier))
        }
        ToChar => {
            fits(text(&a[1]).len().checked_mul(12))?; // as Postgres sizes it: up to 12 bytes a format character
            let char_err = |(message, hint): time::CharErr| super::SqlError {
                code: "22007",
                message,
                info: hint.map(|h| Box::new(super::Violation { hint: Some(h.into()), ..super::Violation::default() })),
            };
            // (an empty template writes null)
            if text(&a[1]).is_empty() && !matches!(&a[0], Int(_) | Float(_) | Numeric(_)) {
                return Ok(Null);
            }
            if let Interval(i) = &a[0] {
                return Ok(Text(time::to_char_interval(*i, &text(&a[1])).map_err(char_err)?));
            }
            // (nothing to write of infinity)
            if match &a[0] {
                Date(d) => time::infinite_date(*d),
                Timestamp(t) | TimestampTz(t) => time::infinite(*t),
                _ => false,
            } {
                return Ok(Null);
            }
            let us = match &a[0] {
                Date(d) => match (*d as i64).checked_mul(time::DAY) {
                    Some(us) => us,
                    None => return err("22008", "timestamp out of range"),
                },
                Timestamp(t) | Time(t) => *t,
                TimestampTz(t) => t.saturating_add(time::offset_at(*t)), // in the session's zone
                v @ (Int(_) | Float(_) | Numeric(_)) => {
                    // (a real's digits: 6, as the binder says with a third argument; else a double's 15)
                    let digits = if matches!(a.get(2), Some(Int(6))) { 6 } else { 15 };
                    return Ok(Text(num_to_char(v, &text(&a[1]), digits)?));
                }
                v => {
                    return err(
                        super::UNSUPPORTED,
                        format!("to_char() formats numbers, dates and times, not {}", v.kind()),
                    );
                }
            };
            // (a timestamptz's TZ, TZH, TZM and OF are the session zone's at that moment)
            let zone = match &a[0] {
                TimestampTz(t) => {
                    let off = (time::offset_at(*t) / 1_000_000) as i32;
                    Some((off, time::session_abbr_at(*t).unwrap_or_else(|| time::fallback_abbr(off))))
                }
                _ => None,
            };
            Text(time::to_char(us, &text(&a[1]), zone.as_ref().map(|(o, n)| (*o, n.as_str()))).map_err(char_err)?)
        }
        Md5 => Text(md5(s().as_bytes())),
        // (as quote_identifier writes it: any keyword but an unreserved one is quoted, `"user"`)
        QuoteIdent => Text(super::collation::quoted(&s())),
        QuoteLiteral => Text(quote_literal(&quoted_text(&a[0])?)),
        // p[i] of a point, line, lseg or box: the binder gives its type's id third (see `geom::subscript`)
        ArrayAt if a.len() == 3 => super::geom::subscript(&a[0], &a[1], &a[2])?,
        // a[i]: arrays count from 1, jsonb arrays from 0 (and take object keys); outside: null
        ArrayAt => match &a[0] {
            Json(serde_json::Value::Object(o)) => o.get(&text(&a[1])).map_or(Null, |v| Json(v.clone())),
            Json(serde_json::Value::Array(xs)) => {
                // (a key that is no number finds nothing in an array)
                let i = match &a[1] {
                    Int(i) => *i,
                    k => match text(k).trim().parse::<i64>() {
                        Ok(i) => i,
                        Err(_) => return Ok(Null),
                    },
                };
                let i = if i < 0 { xs.len() as i64 + i } else { i };
                usize::try_from(i).ok().and_then(|i| xs.get(i)).map_or(Null, |v| Json(v.clone()))
            }
            Json(_) => Null,
            // (of an array of more dimensions, one subscript is too few: null)
            v => {
                let i = int(&a[1])?;
                super::mdarray::read(v)?.map_or(Null, |x| x.get(&[i]))
            }
        },
        // a[i][j]...: an element of an array of that many dimensions (null for one of another number, or a subscript
        // out of range); of a jsonb not typed as one when bound, each step of the one before
        ArrayAtN => match &a[0] {
            Json(_) => {
                let mut v = a[0].clone();
                for k in &a[1..] {
                    v = call(ArrayAt, &[v, k.clone()])?;
                }
                v
            }
            v => {
                let at = a[1..].iter().map(int).collect::<Result<Vec<_>>>()?;
                super::mdarray::read(v)?.map_or(Null, |x| x.get(&at))
            }
        },
        // '' for a number that names no encoding, as in Postgres
        EncodingToChar => {
            Text(usize::try_from(int(&a[0])?).ok().and_then(|i| ENCODINGS.get(i)).map_or("", |e| e).into())
        }
        // (its name compared as Postgres's clean_encoding_name leaves it: letters and digits only, in lower case, so
        // `utf-8` is UTF8)
        CharToEncoding => {
            let clean = |s: &str| s.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_lowercase();
            let name = clean(&s());
            Int(ENCODINGS.iter().position(|e| clean(e) == name).map_or(-1, |i| i as i64))
        }
        // cardinality: all the elements; the others of dimension d, null for one the array doesn't have
        Cardinality | ArrayLength | ArrayLower | ArrayUpper => {
            let x = super::mdarray::read(&a[0])?.unwrap_or_else(super::mdarray::Arr::empty);
            if f == Cardinality {
                return Ok(Int(x.elems.len() as i64));
            }
            let d = usize::try_from(int(&a[1])?.saturating_sub(1)).ok().and_then(|k| x.dims.get(k).copied());
            match (f, d) {
                (_, None) => Null,
                (ArrayLength, Some(d)) => Int(d.len as i64),
                (ArrayLower, Some(d)) => Int(bound(d.lo)?),
                (_, Some(d)) => Int(bound(d.hi())?),
            }
        }
        StMakePoint | StPoint | StSetSrid | StSrid | StGeomFromText | StGeogFromText | StGeomFromGeoJson | StAsText
        | StAsEwkt | StAsGeoJson | StX | StY | GeoX | GeoY | StDistance | StDWithin | StDistanceSphere => {
            return geo(f, a);
        }
        JsonToTsvector => {
            use super::tsearch as ts;
            let (cfg, json, filter) = match a {
                [_, f] => (ts::default_config(), &a[0], f),
                [c, _, f] => (
                    ts::config(&text(c)).map_err(|e| super::SqlError { code: "42704", message: e, info: None })?,
                    &a[1],
                    f,
                ),
                _ => unreachable!(),
            };
            // the flags as `parse_jsonb_index_flags` reads them: a string, or an array of strings
            let filter = super::as_json(filter.clone())?;
            let flag_err = |message: String| -> Result<Value> {
                Err(super::SqlError {
                    code: "22023",
                    message,
                    info: Some(Box::new(super::Violation {
                        hint: Some(
                            "Possible values are: \"string\", \"numeric\", \"boolean\", \"key\", and \"all\".".into(),
                        ),
                        ..Default::default()
                    })),
                })
            };
            let listed: Vec<&serde_json::Value> = match &filter {
                serde_json::Value::Array(l) => l.iter().collect(),
                serde_json::Value::Object(_) => {
                    return err("22023", "wrong flag type, only arrays and scalars are allowed");
                }
                x => vec![x],
            };
            let mut kinds: Vec<String> = vec![];
            for x in listed {
                match x {
                    serde_json::Value::String(k) => {
                        if !["all", "string", "numeric", "boolean", "key"].contains(&k.to_lowercase().as_str()) {
                            return flag_err(format!("wrong flag in flag array: \"{k}\""));
                        }
                        kinds.push(k.clone());
                    }
                    serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
                        return err("XX000", "unexpected end of flag array");
                    }
                    _ => return flag_err("flag array element is not a string".into()),
                }
            }
            let want = |k: &str| kinds.iter().any(|x| x.eq_ignore_ascii_case("all") || x.eq_ignore_ascii_case(k));
            let mut pieces: Vec<String> = vec![];
            fn walk(v: &serde_json::Value, want: &dyn Fn(&str) -> bool, out: &mut Vec<String>) {
                match v {
                    serde_json::Value::String(s) if want("string") => out.push(s.clone()),
                    serde_json::Value::Number(n) if want("numeric") => out.push(n.to_string()),
                    serde_json::Value::Bool(b) if want("boolean") => out.push(b.to_string()),
                    serde_json::Value::Array(l) => l.iter().for_each(|x| walk(x, want, out)),
                    serde_json::Value::Object(m) => {
                        let mut keys: Vec<&String> = m.keys().collect();
                        keys.sort_by(|a, b| (a.len(), a.as_str()).cmp(&(b.len(), b.as_str())));
                        for k in keys {
                            if want("key") {
                                out.push(k.clone());
                            }
                            walk(&m[k], want, out);
                        }
                    }
                    _ => {}
                }
            }
            walk(&super::as_json(json.clone())?, &want, &mut pieces);
            let mut acc = ts::TsVector::default();
            let mut pos: usize = 0;
            for piece in pieces {
                let tv = ts::to_tsvector(cfg, &piece);
                let had_words = !tv.0.is_empty();
                for (lex, ps) in tv.0 {
                    let e = acc.0.entry(lex).or_default();
                    for p in ps {
                        let (w, at) = (p & !((1u16 << 14) - 1), (p & ((1u16 << 14) - 1)) as usize + pos);
                        e.push(w | at.min((1 << 14) - 1) as u16);
                    }
                }
                // (the piece's tokens take their places, stop words too, and a piece that kept words leaves a gap)
                pos += ts::token_count(&piece) + usize::from(had_words);
            }
            Text(acc.to_string())
        }
        // to_tsvector of json or jsonb: the words of its strings, as jsonb_to_tsvector with '"string"'
        ToTsvector if matches!(a.last(), Some(Json(_) | JsonText(_))) => {
            let mut args = a.to_vec();
            args.push(Json(serde_json::Value::String("string".into())));
            return call(JsonToTsvector, &args);
        }
        ToTsvector | ToTsquery | PlaintoTsquery | PhrasetoTsquery | WebsearchToTsquery => {
            use super::tsearch as ts;
            let (cfg, doc) = match a {
                [c, d] => (
                    ts::config(&text(c)).map_err(|e| super::SqlError { code: "42704", message: e, info: None })?,
                    text(d),
                ),
                [d] => (ts::default_config(), text(d)), // the session's default_text_search_config
                _ => unreachable!(),
            };
            ts::size_ok(&doc).map_err(|e| super::SqlError { code: "54000", message: e, info: None })?;
            let q = match f {
                ToTsvector => return Ok(Text(ts::to_tsvector(cfg, &doc).to_string())),
                // (nothing to parse: Postgres's notice, and the empty query)
                ToTsquery if doc.trim().is_empty() => {
                    super::notice("NOTICE", format!("text-search query doesn't contain lexemes: \"{doc}\""));
                    None
                }
                ToTsquery => ts::to_tsquery(cfg, &doc).map_err(|e| super::SqlError {
                    code: super::SYNTAX,
                    message: e,
                    info: None,
                })?,
                PlaintoTsquery => ts::plainto_tsquery(cfg, &doc),
                PhrasetoTsquery => ts::phraseto_tsquery(cfg, &doc).map_err(|e| super::SqlError {
                    code: "54000",
                    message: e,
                    info: None,
                })?,
                _ => {
                    let (q, emptied) = ts::websearch_to_tsquery(cfg, &doc).map_err(|e| super::SqlError {
                        code: "54000",
                        message: e,
                        info: None,
                    })?;
                    if emptied {
                        super::notice(
                            "NOTICE",
                            "text-search query contains only stop words or doesn't contain lexemes, ignored".into(),
                        );
                    }
                    q
                }
            };
            Text(ts::Query::text(&q))
        }
        TsRank | TsRankCd => {
            use super::tsearch as ts;
            // ts_rank([weights,] vector, query [, normalization])
            let (weights, rest) = match &a[0] {
                Text(w) if w.starts_with('{') => {
                    let w: Vec<f32> = super::array_values(Text(w.clone()))?
                        .unwrap_or_default()
                        .iter()
                        .map(|v| float(v).map(|f| f as f32))
                        .collect::<Result<_>>()?;
                    let Ok(w) = <[f32; 4]>::try_from(w) else { return err("2202E", "array of weight is too short") };
                    ([w[0], w[1], w[2], w[3]], &a[1..])
                }
                _ => (ts::default_weights(), a),
            };
            let (Some(v), Some(q)) = (rest.first(), rest.get(1)) else {
                return err(super::NO_FUNCTION, "ts_rank(vector, query [, normalization])");
            };
            let method = match rest.get(2) {
                Some(m) => int(m)? as i32,
                None => 0,
            };
            let (vec, query) = (super::ts_vector(&text(v))?, super::ts_query(&text(q))?);
            let r = if f == TsRankCd {
                ts::rank_cd(&weights, &vec, &query, method)?
            } else {
                ts::rank(&weights, &vec, &query, method)?
            };
            // a real: shown as Postgres shows it (its shortest text as a float4, whichever type the column is told)
            let r = r as f32;
            Float(r.to_string().parse().unwrap_or(f64::from(r)))
        }
        TsConcat => Text(super::ts_vector(&s())?.concat(&super::ts_vector(&text(&a[1]))?).to_string()),
        TsHeadline => {
            use super::tsearch as ts;
            // ts_headline([config,] document, query [, options])
            let by_config = match a.len() {
                4 => true,
                3 => is_config(&text(&a[0])) && super::ts_query(&text(&a[2])).is_ok(),
                _ => false,
            };
            let (cfg, rest) = if by_config {
                (
                    ts::config(&text(&a[0])).map_err(|e| super::SqlError { code: "42704", message: e, info: None })?,
                    &a[1..],
                )
            } else {
                (ts::default_config(), a)
            };
            let (doc, query) = (text(&rest[0]), super::ts_query(&text(&rest[1]))?);
            let options = rest.get(2).map(text).unwrap_or_default();
            Text(ts::headline(cfg, &doc, &query, &options).map_err(|e| super::SqlError {
                code: "22023",
                message: e,
                info: None,
            })?)
        }
        TsDelete | TsFilter | ArrayToTsvector | TsvectorToArray | NumNode | QueryTree | TsqueryPhrase | TsqueryAnd
        | TsqueryOr | TsqueryNot | TsqueryCmp | TsqueryContains | TsRewrite => {
            use super::tsearch as ts;
            let list = |v: &Value| -> Result<Vec<Value>> {
                super::array_values(v.clone())?.ok_or_else(|| super::SqlError {
                    code: "22004",
                    message: "null array".into(),
                    info: None,
                })
            };
            match f {
                TsDelete => {
                    let mut v = super::ts_vector(&s())?;
                    let drop: Vec<String> = match &a[1] {
                        Text(t) if t.starts_with('{') => list(&a[1])?.iter().map(|x| x.to_text()).collect(),
                        x => vec![text(x)],
                    };
                    v.0.retain(|k, _| !drop.contains(k));
                    Text(v.to_string())
                }
                TsFilter => {
                    let mut v = super::ts_vector(&s())?;
                    let mut mask = 0u8;
                    for w in list(&a[1])? {
                        if w == Null {
                            return err("22004", "weight array may not contain nulls");
                        }
                        mask |= match text(&w).to_ascii_uppercase().as_str() {
                            "A" => 8,
                            "B" => 4,
                            "C" => 2,
                            "D" => 1,
                            w => return err("22023", format!("unrecognized weight: \"{w}\"")),
                        };
                    }
                    for ps in v.0.values_mut() {
                        ps.retain(|p| mask & (1 << (p >> 14)) != 0);
                    }
                    v.0.retain(|_, ps| !ps.is_empty());
                    Text(v.to_string())
                }
                ArrayToTsvector => {
                    let mut v = ts::TsVector::default();
                    for x in list(&a[0])? {
                        if x == Null {
                            return err("22004", "lexeme array may not contain nulls");
                        }
                        let t = text(&x);
                        if t.is_empty() {
                            return err("22023", "lexeme array may not contain empty strings");
                        }
                        v.0.insert(t, vec![]);
                    }
                    Text(v.to_string())
                }
                TsvectorToArray => Text(super::array_text(
                    super::ts_vector(&s())?.0.keys().map(|k| Text(k.clone())).collect::<Vec<_>>().iter(),
                )),
                NumNode => {
                    fn count(q: &ts::Query) -> i64 {
                        match q {
                            ts::Query::Val { .. } => 1,
                            ts::Query::Not(a) => 1 + count(a),
                            ts::Query::And(a, b) | ts::Query::Or(a, b) | ts::Query::Phrase(a, b, _) => {
                                1 + count(a) + count(b)
                            }
                        }
                    }
                    Int(super::ts_query(&s())?.as_ref().map_or(0, count))
                }
                QueryTree => {
                    // the query without what it asks to be absent
                    fn clean(q: ts::Query) -> Option<ts::Query> {
                        use ts::Query::*;
                        match q {
                            Val { .. } => Some(q),
                            Not(_) => None,
                            And(a, b) => match (clean(*a), clean(*b)) {
                                (Some(x), Some(y)) => Some(And(Box::new(x), Box::new(y))),
                                (x, y) => x.or(y),
                            },
                            Or(a, b) => match (clean(*a), clean(*b)) {
                                (Some(x), Some(y)) => Some(Or(Box::new(x), Box::new(y))),
                                _ => None,
                            },
                            Phrase(a, b, n) => match (clean(*a), clean(*b)) {
                                (Some(x), Some(y)) => Some(Phrase(Box::new(x), Box::new(y), n)),
                                (x, y) => x.or(y),
                            },
                        }
                    }
                    match super::ts_query(&s())? {
                        None => Text(String::new()),
                        Some(q) => match clean(q) {
                            Some(q) => Text(ts::Query::text(&Some(q))),
                            None => Text("T".into()),
                        },
                    }
                }
                TsqueryPhrase => {
                    let n = match a.get(2) {
                        Some(d) => int(d)?,
                        None => 1,
                    };
                    let (x, y) = (super::ts_query(&s())?, super::ts_query(&text(&a[1]))?);
                    Text(ts::Query::text(&match (x, y) {
                        (Some(x), Some(y)) => Some(ts::Query::Phrase(Box::new(x), Box::new(y), n as u16)),
                        (x, y) => x.or(y),
                    }))
                }
                // q && r, q || r: an empty side gives the other (tsquery_and, tsquery_or)
                TsqueryAnd | TsqueryOr => {
                    let (x, y) = (super::ts_query(&s())?, super::ts_query(&text(&a[1]))?);
                    Text(ts::Query::text(&match (x, y) {
                        (Some(x), Some(y)) if f == TsqueryAnd => Some(ts::Query::And(Box::new(x), Box::new(y))),
                        (Some(x), Some(y)) => Some(ts::Query::Or(Box::new(x), Box::new(y))),
                        (x, y) => x.or(y),
                    }))
                }
                TsqueryNot => Text(ts::Query::text(&super::ts_query(&s())?.map(|q| ts::Query::Not(Box::new(q))))),
                TsqueryCmp => {
                    let (x, y) = (super::ts_query(&s())?, super::ts_query(&text(&a[1]))?);
                    Int(ts::compare_queries(&x, &y) as i64)
                }
                TsqueryContains => {
                    let (x, y) = (super::ts_query(&s())?, super::ts_query(&text(&a[1]))?);
                    Bool(ts::contains(&x, &y))
                }
                _ => {
                    // ts_rewrite(query, target, substitute): the target's place in the query taken by the substitute
                    let (q, target, sub) =
                        (super::ts_query(&s())?, super::ts_query(&text(&a[1]))?, super::ts_query(&text(&a[2]))?);
                    Text(ts::Query::text(&ts::rewrite(q, &[(target, sub)], true)))
                }
            }
        }
        Setweight | Strip => {
            let mut v = super::ts_vector(&s())?;
            if f == Setweight && a.len() == 3 {
                // setweight(vector, weight, lexemes): only those lexemes
                let w = match text(&a[1]).to_ascii_uppercase().as_str() {
                    "A" => 3u16,
                    "B" => 2,
                    "C" => 1,
                    "D" => 0,
                    w => return err("22023", format!("unrecognized weight: \"{w}\"")),
                };
                let names: Vec<String> =
                    super::array_values(a[2].clone())?.unwrap_or_default().iter().map(text).collect();
                for (k, ps) in v.0.iter_mut() {
                    if names.contains(k) {
                        for p in ps.iter_mut() {
                            *p = (*p & ((1 << 14) - 1)) | (w << 14);
                        }
                    }
                }
                return Ok(Text(v.to_string()));
            }
            if f == Strip {
                v.strip();
            } else {
                let w = match text(&a[1]).to_ascii_uppercase().as_str() {
                    "A" => 3,
                    "B" => 2,
                    "C" => 1,
                    "D" => 0,
                    w => return err("22023", format!("unrecognized weight: \"{w}\"")),
                };
                v.setweight(w);
            }
            Text(v.to_string())
        }
        LikeEscape => match text(&a[1]) {
            e if e.chars().count() > 1 => return Err(bad_escape()),
            e => Text(like_escape(&s(), e.chars().next())),
        },
        SimilarEscape => match a.get(1).map(text) {
            Some(e) if e.chars().count() > 1 => return Err(bad_escape()),
            e => Text(similar_escape(&s(), e.map_or(Some('\\'), |e| e.chars().next()))?),
        },
        RegexpInstr | RegexpCount | RegexpSubstr | RegexpReplace
            if f == RegexpInstr
                || a.len() > if f == RegexpReplace { 4 } else { 2 }
                || (f == RegexpReplace && a.len() == 4 && matches!(a[3], Value::Int(_))) =>
        {
            regexp_ext(f, a)?
        }
        RegexpReplace | RegexpMatch | RegexpLike | RegexpCount | RegexpSubstr | RegexpSplit | RegexpInstr => {
            let flags = match f {
                RegexpReplace => a.get(3),
                RegexpCount | RegexpSubstr => None,
                _ => a.get(2),
            };
            // (regexp_replace(s, p, r, '1'): a start meant, given as text)
            if f == RegexpReplace
                && let Some(c) = flags.map(text).and_then(|t| t.chars().next()).filter(char::is_ascii_digit)
            {
                return Err(super::SqlError {
                    code: "22023",
                    message: format!("invalid regular expression option: \"{c}\""),
                    info: Some(Box::new(super::Violation {
                        hint: Some(
                            "If you meant to use regexp_replace() with a start parameter, cast the fourth argument to \
                             integer explicitly."
                                .into(),
                        ),
                        ..super::Violation::default()
                    })),
                });
            }
            let (flags, global) = super::regex_flags(flags)?;
            if global && matches!(f, RegexpLike | RegexpSplit | RegexpCount | RegexpSubstr | RegexpInstr) {
                return err("22023", format!("{}() does not support the \"global\" option", regexp_name(f)));
            }
            if global && f == RegexpMatch {
                return Err(super::SqlError {
                    code: "22023",
                    message: "regexp_match() does not support the \"global\" option".into(),
                    info: Some(Box::new(super::Violation {
                        hint: Some("Use the regexp_matches function instead.".into()),
                        ..super::Violation::default()
                    })),
                });
            }
            let re = super::compiled(&text(&a[1]), &flags)?;
            let s = s();
            match f {
                RegexpReplace => Text(super::regex::replace(&re, &s, &text(&a[2]), global)?),
                RegexpLike => Bool(re.is_match(&s)?),
                RegexpSplit => {
                    Text(super::array_literal(&super::regex::split(&re, &s)?.into_iter().map(Text).collect::<Vec<_>>()))
                }
                RegexpMatch => match super::regex::first_match(&re, &s)? {
                    Some(groups) => Text(super::array_literal(
                        &groups.into_iter().map(|g| g.map_or(Null, Text)).collect::<Vec<_>>(),
                    )),
                    None => Null,
                },
                _ => {
                    let chars = chars(&s)?;
                    let (mut n, mut at, mut first) = (0, 0, None);
                    while let Some(caps) = re.find_at(&chars, at)? {
                        let (ms, me) = caps[0].unwrap();
                        first.get_or_insert((ms, me));
                        n += 1;
                        if ms >= chars.len() {
                            break;
                        }
                        at = if me == ms { me + 1 } else { me };
                    }
                    match f {
                        RegexpCount => Int(n),
                        _ => first.map_or(Null, |(x, y)| Text(chars[x..y].iter().collect())),
                    }
                }
            }
        }
        // a datum's size: a fixed-width type's width, else its text after a varlena's header (jsonb's text stands
        // for its binary form: equal values are as long)
        ColumnSize => Int(match &a[0] {
            Int(n) => {
                if i32::try_from(*n).is_ok() {
                    4
                } else {
                    8
                }
            }
            Float(_) | Timestamp(_) | TimestampTz(_) | Time(_) => 8,
            Bool(_) => 1,
            Date(_) => 4,
            Interval(_) => 16,
            Bytea(b) => b.len() as i64 + 4,
            v => v.to_text().len() as i64 + 4,
        }),
        Overlaps | Concat | ConcatWs | Greatest | Least | Nullif | Random | RandomNormal | UuidV1(_) | Pi
        | JsonBuildObject | JsonBuildArray | JsonCtor | JsonFormatted | ArrayOf | BigintOid | StringToArray
        | Format | QuoteNullable | Spelled(_) | JsonOpFn(_) => {
            unreachable!("handled above")
        }
    })
}

/// `(s1, e1) OVERLAPS (s2, e2)` as Postgres works it out: each period is put in order, and a null
/// end makes a period start-only, so the answer is null only where the nulls decide it.
/// Whether a function is one the network types' binder checks (see `Binder::net_function`): theirs, or
/// `trunc`, which a macaddr has as well as a number.
pub(super) fn net_kind(f: Scalar) -> bool {
    use Scalar::*;
    matches!(
        f,
        NetHost
            | NetAbbrev
            | NetBroadcast
            | NetNetmask
            | NetHostmask
            | NetNetwork
            | NetMasklen
            | NetFamily
            | NetSetMasklen
            | NetSameFamily
            | NetMerge
            | NetSet7bit
            | NetInet
            | NetCidr
            | NetMacaddr
            | NetMacaddr8
            | Trunc
    )
}

/// A network value from an argument: one already, or a text (a literal or a parameter, which
/// Postgres reads as the type the function takes) read as `kind`.
pub(super) fn net_arg(v: &Value, kind: net::Kind, name: &str) -> Result<net::Net> {
    match v {
        Value::Net(n) => Ok(n.clone()),
        Value::Text(s) => net::parse(kind, s),
        v => err(super::NO_FUNCTION, format!("function {name}({}) does not exist", v.kind())),
    }
}

/// The network address functions: `a` has no null in it. None for a function that is not one, or
/// a `trunc` of something that is not a macaddr.
fn net_call(f: Scalar, a: &[Value]) -> Result<Option<Value>> {
    use Scalar::*;
    use net::Kind;
    let (name, kind) = match f {
        NetHost => ("host", Kind::Inet),
        NetAbbrev => ("abbrev", Kind::Inet),
        NetBroadcast => ("broadcast", Kind::Inet),
        NetNetmask => ("netmask", Kind::Inet),
        NetHostmask => ("hostmask", Kind::Inet),
        NetNetwork => ("network", Kind::Inet),
        NetMasklen => ("masklen", Kind::Inet),
        NetFamily => ("family", Kind::Inet),
        NetSetMasklen => ("set_masklen", Kind::Inet),
        NetSameFamily => ("inet_same_family", Kind::Inet),
        NetMerge => ("inet_merge", Kind::Inet),
        NetSet7bit => ("macaddr8_set7bit", Kind::Mac8),
        NetInet => ("inet", Kind::Inet),
        NetCidr => ("cidr", Kind::Cidr),
        NetMacaddr => ("macaddr", Kind::Mac),
        NetMacaddr8 => ("macaddr8", Kind::Mac8),
        Trunc if matches!(a[0], Value::Net(_)) => ("trunc", Kind::Mac),
        _ => return Ok(None),
    };
    let x = net_arg(&a[0], kind, name)?;
    let out = |r: Result<net::Net>| r.map(|n| Some(Value::Net(n)));
    match f {
        NetHost => net::host(&x).map(|s| Some(Value::Text(s))),
        NetAbbrev => net::abbrev(&x).map(|s| Some(Value::Text(s))),
        NetBroadcast => out(net::broadcast(&x)),
        NetNetmask => out(net::netmask(&x)),
        NetHostmask => out(net::hostmask(&x)),
        NetNetwork => out(net::network(&x)),
        NetMasklen => net::masklen(&x).map(|n| Some(Value::Int(n.into()))),
        NetFamily => net::family(&x).map(|n| Some(Value::Int(n.into()))),
        NetSetMasklen => {
            // (the length is an int4: a bigger one is a call of set_masklen(inet, bigint))
            let Some(bits) = int(&a[1]).ok().and_then(|n| i32::try_from(n).ok()) else {
                return err(
                    super::NO_FUNCTION,
                    format!("function set_masklen({}, bigint) does not exist", x.kind().name()),
                );
            };
            out(net::set_masklen(&x, bits))
        }
        NetSameFamily => net::inet_same_family(&x, &net_arg(&a[1], kind, name)?).map(|b| Some(Value::Bool(b))),
        NetMerge => out(net::inet_merge(&x, &net_arg(&a[1], kind, name)?)),
        NetSet7bit => out(net::macaddr8_set7bit(&x)),
        NetInet | NetCidr | NetMacaddr | NetMacaddr8 => out(net::convert(&x, kind, net::Syntax::Call)),
        _ => out(net::trunc(&x)),
    }
}

fn overlaps(a: &[Value]) -> Result<Value> {
    let end = |s: &Value, e: &Value| match (s, e) {
        (Value::Interval(_), _) => err(super::NO_OPERATOR, "OVERLAPS takes an interval only as an end"),
        (s, e @ Value::Interval(_)) if *s != Value::Null => super::binop(super::Op::Add, s.clone(), e.clone()),
        (_, e) => Ok(e.clone()),
    };
    if let Some(v) =
        a.iter().find(|v| matches!(v, Value::Int(_) | Value::Float(_) | Value::Numeric(_) | Value::Bool(_)))
    {
        return err(super::NO_FUNCTION, format!("function overlaps({}, ...) does not exist", v.kind()));
    }
    let less = |x: &Value, y: &Value| super::compare(x, y).map(|o| o == std::cmp::Ordering::Less);
    // (start, end): the end None if it is null; the start is never null
    let period = |s: &Value, e: &Value| -> Result<Option<(Value, Option<Value>)>> {
        let e = end(s, e)?;
        Ok(match (s == &Value::Null, e == Value::Null) {
            (true, true) => None,
            (true, false) => Some((e, None)),
            (false, true) => Some((s.clone(), None)),
            (false, false) if less(&e, s)? => Some((e, Some(s.clone()))),
            (false, false) => Some((s.clone(), Some(e))),
        })
    };
    let (Some((s1, e1)), Some((s2, e2))) = (period(&a[0], &a[1])?, period(&a[2], &a[3])?) else {
        return Ok(Value::Null);
    };
    // Postgres's rule by which start is later (equal starts always overlap)
    let (last, e_first, e_last) = if less(&s2, &s1)? {
        (&s1, &e2, &e1)
    } else if less(&s1, &s2)? {
        (&s2, &e1, &e2)
    } else {
        return Ok(Value::Bool(true));
    };
    // they overlap if the later start is before the earlier period's end
    match e_first {
        None => Ok(Value::Null),
        Some(end) if less(last, end)? => Ok(Value::Bool(true)),
        Some(_) if e_last.is_none() => Ok(Value::Null),
        Some(_) => Ok(Value::Bool(false)),
    }
}

/// A double as C's `%g` writes it: six significant digits, in exponent form below 1e-4 or from 1e6.
fn float_g(v: f64) -> String {
    if v == 0.0 || !v.is_finite() {
        return format!("{v}");
    }
    let exp = v.abs().log10().floor() as i32;
    // (rounding to six digits can carry into the next power of ten)
    let rounded: f64 = format!("{v:.5e}").parse().unwrap_or(v);
    let exp = if rounded.abs() >= 10f64.powi(exp + 1) { exp + 1 } else { exp };
    let trim = |s: String| if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s };
    if !(-4..6).contains(&exp) {
        let s = format!("{v:.5e}");
        let (m, e) = s.split_once('e').unwrap_or((&s, "0"));
        let e: i32 = e.parse().unwrap_or(0);
        format!("{}e{}{:02}", trim(m.to_string()), if e < 0 { '-' } else { '+' }, e.abs())
    } else {
        trim(format!("{:.*}", (5 - exp).max(0) as usize, v))
    }
}

/// A to_date/to_timestamp failure as Postgres reports it: `message: Detail.` is the message and its DETAIL, and
/// ` Hint: ...` after it its HINT.
fn date_format_error(code: &'static str, m: String) -> super::SqlError {
    let (main, hint) = match m.split_once(" Hint: ") {
        Some((a, h)) => (a.to_string(), Some(h.to_string())),
        None => (m, None),
    };
    // (a detail is a sentence after ": "; a quoted input after it is part of the message)
    let at = main.rfind(": ").filter(|i| main[i + 2..].starts_with(|c: char| c.is_ascii_uppercase()));
    let (message, detail) = match at {
        Some(i) => (main[..i].to_string(), Some(main[i + 2..].to_string())),
        None => (main.trim_end_matches('.').to_string(), None),
    };
    let info = (detail.is_some() || hint.is_some())
        .then(|| Box::new(super::Violation { detail, hint, ..super::Violation::default() }));
    super::SqlError { code, message, info }
}

/// Postgres's `RADIANS_PER_DEGREE`, which `degrees` divides by and `radians` multiplies by.
const RADIANS_PER_DEGREE: f64 = 0.017_453_292_519_943_295;

/// An integer or numeric argument as a numeric (what `div` takes: floats aren't accepted, as in
/// Postgres, which has no `div(double precision, ...)`).
fn exact(v: &Value) -> Result<super::decimal::Dec> {
    match v {
        Value::Int(n) => Ok(super::decimal::Dec::int(*n)),
        Value::Numeric(d) => Ok(d.clone()),
        // (a bare string is read as a numeric, as Postgres resolves an unknown argument)
        Value::Text(_) => match super::coerce(v.clone(), Type::Numeric)? {
            Value::Numeric(d) => Ok(d),
            _ => err(super::BAD_INPUT, "invalid input syntax for type numeric"),
        },
        v => err(super::NO_FUNCTION, format!("function div({}, ...) does not exist", v.kind())),
    }
}

/// `width_bucket(operand, thresholds)` (arrayfuncs.c's width_bucket_array): how many of the sorted thresholds the
/// operand is at or past, found by halving; each threshold read as the operand's type is (a NaN past every number).
fn width_bucket_array(a: &[Value]) -> Result<Value> {
    let Some(arr) = super::mdarray::read(&a[1])? else { return Ok(Value::Null) };
    if arr.dims.len() > 1 {
        return err("2202E", "thresholds must be one-dimensional array");
    }
    if arr.elems.contains(&Value::Null) {
        return err("22004", "thresholds array must not contain NULLs");
    }
    let ty = match &a[0] {
        Value::Int(_) | Value::Numeric(_) => Some(Type::Numeric),
        Value::Float(_) => Some(Type::Float),
        Value::Timestamp(_) => Some(Type::Timestamp),
        Value::TimestampTz(_) => Some(Type::TimestampTz),
        Value::Date(_) => Some(Type::Date),
        Value::Interval(_) => Some(Type::Interval),
        Value::Time(_) => Some(Type::Time),
        _ => None,
    };
    let op = match (ty, &a[0]) {
        (Some(Type::Numeric), v @ Value::Int(_)) => super::coerce(v.clone(), Type::Numeric)?,
        (_, v) => v.clone(),
    };
    let thresholds: Vec<Value> = match ty {
        Some(t) => arr.elems.into_iter().map(|e| super::coerce(e, t)).collect::<Result<_>>()?,
        None => arr.elems,
    };
    let (mut left, mut right) = (0usize, thresholds.len());
    while left < right {
        let mid = (left + right) / 2;
        if super::compare(&op, &thresholds[mid])?.is_lt() {
            right = mid;
        } else {
            left = mid + 1;
        }
    }
    Ok(Value::Int(left as i64))
}

/// `width_bucket(operand, low, high, count)`: which of `count` equal-width buckets between the
/// bounds the operand is in (0 below, count + 1 at or past the high end), counting down when the
/// bounds are given high to low. Exact for integers and numerics, on doubles if any is one.
fn width_bucket(a: &[Value]) -> Result<Value> {
    let count = int(&a[3])?;
    if count <= 0 {
        return err("2201G", "count must be greater than zero");
    }
    let past = || {
        i32::try_from(count + 1)
            .map(|n| Value::Int(i64::from(n)))
            .or_else(|_| err(super::RANGE, "integer out of range"))
    };
    let floats = a[..3].iter().any(|v| matches!(v, Value::Float(_)));
    if floats {
        let (op, lo, hi) = (float(&a[0])?, float(&a[1])?, float(&a[2])?);
        if op.is_nan() || lo.is_nan() || hi.is_nan() {
            return err("2201G", "operand, lower bound, and upper bound cannot be NaN");
        }
        if lo.is_infinite() || hi.is_infinite() {
            return err("2201G", "lower and upper bounds must be finite");
        }
        // (width_bucket_float8: halved where the bounds' distance overflows, and never past the last bucket)
        let (below, beyond) = if lo < hi { (op < lo, op >= hi) } else { (op > lo, op <= hi) };
        if lo == hi {
            return err("2201G", "lower bound cannot equal upper bound");
        }
        if below {
            return Ok(Value::Int(0));
        }
        if beyond {
            return past();
        }
        let n = count as f64;
        let q = if lo < hi {
            if (hi - lo).is_finite() {
                n * ((op - lo) / (hi - lo))
            } else {
                n * ((op / 2.0 - lo / 2.0) / (hi / 2.0 - lo / 2.0))
            }
        } else if (lo - hi).is_finite() {
            n * ((lo - op) / (lo - hi))
        } else {
            n * ((lo / 2.0 - op / 2.0) / (lo / 2.0 - hi / 2.0))
        };
        let b = (q as i64).min(count - 1);
        return Ok(Value::Int(b + 1));
    }
    // width_bucket_numeric: exact, the operand may be infinite
    use super::decimal::Dec;
    use std::cmp::Ordering;
    let d = |v: &Value| match v {
        Value::Text(_) => exact(v),
        v => super::num(v.clone()).and_then(|v| exact(&v)),
    };
    let (x, l, h) = (d(&a[0])?, d(&a[1])?, d(&a[2])?);
    if x.is_nan() || l.is_nan() || h.is_nan() {
        return err("2201G", "operand, lower bound, and upper bound cannot be NaN");
    }
    if l.is_special() || h.is_special() {
        return err("2201G", "lower and upper bounds must be finite");
    }
    let reversed = match Dec::cmp(&l, &h) {
        Ordering::Equal => return err("2201G", "lower bound cannot equal upper bound"),
        o => o == Ordering::Greater,
    };
    let (below, beyond) = if reversed {
        (Dec::cmp(&x, &l) == Ordering::Greater, Dec::cmp(&x, &h) != Ordering::Greater)
    } else {
        (Dec::cmp(&x, &l) == Ordering::Less, Dec::cmp(&x, &h) != Ordering::Less)
    };
    if below {
        return Ok(Value::Int(0));
    }
    if beyond {
        return past();
    }
    // compute_bucket: (x - low) * count / (high - low) at a quotient's scale, then one more, rounded down (and
    // never past count, which a rounded quotient could reach)
    let range = |e: &str| super::SqlError { code: super::decimal::sqlstate(e), message: e.into(), info: None };
    let (at, width) = if reversed {
        (Dec::sub(&l, &x).map_err(range)?, Dec::sub(&l, &h).map_err(range)?)
    } else {
        (Dec::sub(&x, &l).map_err(range)?, Dec::sub(&h, &l).map_err(range)?)
    };
    let n = Dec::int(count);
    let q = Dec::div(Dec::mul(at, &n).map_err(range)?, width).map_err(range)?;
    let bucket = if Dec::cmp(&q, &n) != Ordering::Less {
        n
    } else {
        Dec::add(q, Dec::int(1)).and_then(|b| b.floor()).map_err(range)?
    };
    match bucket.to_i64().and_then(|b| i32::try_from(b).ok()) {
        Some(b) => Ok(Value::Int(i64::from(b))),
        None => err(super::RANGE, "integer out of range"),
    }
}

/// Postgres's quote_literal: in quotes, quotes doubled, and with backslashes doubled behind `E`.
fn quote_literal(s: &str) -> String {
    let q = s.replace('\'', "''");
    if s.contains('\\') { format!("E'{}'", q.replace('\\', "\\\\")) } else { format!("'{q}'") }
}

/// Postgres's format(): `%[n$][-][width | * | *n$]` then `s` (text; null is empty), `I` (an
/// identifier, quoted as needed) or `L` (a literal; null is NULL), and `%%`.
fn format(f: &str, args: &[Value]) -> Result<String> {
    use Value::Null;
    let c = chars(f)?;
    let (mut out, mut i, mut next) = (String::new(), 0, 0usize);
    let few = || err("22023", "too few arguments for format()");
    // (with Postgres's hint: a lone % was meant)
    let percent_hint = |message: String| -> Result<String> {
        Err(super::SqlError {
            code: "22023",
            message,
            info: Some(Box::new(super::Violation {
                hint: Some("For a single \"%\" use \"%%\".".into()),
                ..super::Violation::default()
            })),
        })
    };
    let unterminated = || percent_hint("unterminated format() type specifier".into());
    // a number, and whether `$` follows it (it's then taken too)
    let number = |i: &mut usize| -> Result<Option<(usize, bool)>> {
        let start = *i;
        while c.get(*i).is_some_and(char::is_ascii_digit) {
            *i += 1;
        }
        if *i == start {
            return Ok(None);
        }
        let n: usize = c[start..*i].iter().collect::<String>().parse().map_err(|_| super::SqlError {
            code: "22003",
            message: "number is out of range".into(),
            info: None,
        })?;
        let dollar = c.get(*i) == Some(&'$');
        *i += dollar as usize;
        Ok(Some((n, dollar)))
    };
    let arg = |n: usize| -> Result<usize> {
        if n == 0 {
            return err("22023", "format specifies argument 0, but arguments are numbered from 1");
        }
        Ok(n - 1)
    };
    while i < c.len() {
        if c[i] != '%' {
            out.push(c[i]);
            i += 1;
            continue;
        }
        i += 1;
        match c.get(i) {
            None => return unterminated(),
            Some('%') => {
                out.push('%');
                i += 1;
                continue;
            }
            _ => {}
        }
        let (mut at, mut width, mut left) = (None, None, false);
        let back = i;
        match number(&mut i)? {
            Some((n, true)) => at = Some(arg(n)?),
            Some(_) => i = back, // a width, read below
            None => {}
        }
        if c.get(i) == Some(&'-') {
            left = true;
            i += 1;
        }
        if c.get(i) == Some(&'*') {
            i += 1;
            let w = match number(&mut i)? {
                Some((n, true)) => arg(n)?,
                Some(_) => return unterminated(),
                None => {
                    next += 1;
                    next - 1
                }
            };
            next = w + 1;
            width = Some(match args.get(w).ok_or(()).or_else(|_| few())? {
                Null => 0,
                v => int(v)?,
            });
        } else if let Some((n, dollar)) = number(&mut i)? {
            if dollar {
                return unterminated();
            }
            width = Some(n as i64);
        }
        let Some(&ty) = c.get(i) else { return unterminated() };
        i += 1;
        let k = at.unwrap_or(next);
        next = k + 1;
        let v = args.get(k).ok_or(()).or_else(|_| few())?;
        let s = match (ty, v) {
            ('s', Null) => String::new(),
            ('s', v) => text(v),
            ('I', Null) => return err("22004", "null values cannot be formatted as an SQL identifier"),
            ('I', v) => super::ident(&text(v)),
            ('L', Null) => "NULL".into(),
            ('L', v) => quote_literal(&text(v)),
            (t, _) => return percent_hint(format!("unrecognized format() type specifier \"{t}\"")),
        };
        let w = width.unwrap_or(0);
        let (left, w) = (left || w < 0, usize::try_from(w.unsigned_abs()).unwrap_or(usize::MAX));
        let spaces = w.saturating_sub(s.chars().count());
        fits(out.len().checked_add(s.len()).and_then(|n| n.checked_add(spaces)))?;
        let pad = " ".repeat(spaces);
        if left {
            out.push_str(&s);
            out.push_str(&pad);
        } else {
            out.push_str(&pad);
            out.push_str(&s);
        }
    }
    Ok(out)
}

/// `to_char(n, '9.999EEEE')`: scientific notation, with as many digits after the point as the format has.
fn num_scientific(v: &Value, low: &str) -> Result<String> {
    let fm = low.contains("fm");
    let post = low.split_once(['.', 'd']).map_or(0, |(_, r)| r.chars().take_while(|c| matches!(c, '9' | '0')).count());
    let sign = |neg: bool| {
        if neg {
            "-"
        } else if fm {
            ""
        } else {
            " "
        }
    };
    let hashes = || format!("{}#.{}####", sign(false), "#".repeat(post));
    // the number's digits with its point taken out, and how many of them come before the point
    let (neg, digits, before) = match v {
        Value::Float(x) if !x.is_finite() => return Ok(hashes()),
        Value::Numeric(d) if d.is_special() => return Ok(hashes()),
        Value::Float(x) => {
            let t = format!("{:e}", x.abs());
            let (m, e) = t.split_once('e').unwrap_or((&t, "0"));
            let e: i64 = e.parse().unwrap_or(0);
            (*x < 0.0, m.replace('.', ""), e + 1)
        }
        v => {
            let t = v.to_text();
            let t = t.trim_start_matches('-').to_string();
            let (i, f) = t.split_once('.').unwrap_or((&t, ""));
            (v.to_text().starts_with('-'), format!("{i}{f}"), i.len() as i64)
        }
    };
    let lead = digits.bytes().position(|b| b != b'0');
    let Some(lead) = lead else {
        return Ok(format!("{}0.{}e+00", sign(false), "0".repeat(post)));
    };
    let mut exp = before - lead as i64 - 1;
    // one digit, the point, `post` more: rounded half up on the digit after
    let mut m: Vec<u8> =
        digits.bytes().skip(lead).chain(std::iter::repeat(b'0')).take(post + 2).map(|b| b - b'0').collect();
    let up = m.pop().unwrap_or(0) >= 5;
    if up {
        let mut k = m.len();
        loop {
            if k == 0 {
                m.insert(0, 1);
                m.pop();
                exp += 1;
                break;
            }
            k -= 1;
            if m[k] == 9 {
                m[k] = 0;
            } else {
                m[k] += 1;
                break;
            }
        }
    }
    let text: String = m.iter().map(|d| (b'0' + d) as char).collect();
    let frac = if post > 0 { format!(".{}", &text[1..]) } else { String::new() };
    Ok(format!("{}{}{frac}e{}{:02}", sign(neg), &text[..1], if exp < 0 { '-' } else { '+' }, exp.abs()))
}

/// `to_char(n, 'RN')`: a Roman numeral, for 1 to 3999, in 15 characters unless FM.
fn num_roman(v: &Value, fmt: &str) -> Result<String> {
    let fm = fmt.to_ascii_lowercase().contains("fm");
    let lower = fmt.contains("rn");
    let n = match v {
        Value::Float(x) => x.round() as i64,
        Value::Int(n) => *n,
        v => int(&super::coerce(v.clone(), super::Type::BigInt)?)?,
    };
    if !(1..=3999).contains(&n) {
        return Ok("#".repeat(15));
    }
    let table = [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let (mut left, mut out) = (n, String::new());
    for (value, digits) in table {
        while left >= value {
            out.push_str(digits);
            left -= value;
        }
    }
    let out = if lower { out.to_lowercase() } else { out };
    Ok(if fm { out } else { format!("{out:>15}") })
}

/// `to_char` for numbers, as Postgres's formatting.c does it (NUMDesc_prepare, NUM_processor and
/// NUM_numpart_to_char, ported step by step): digits `9` and `0`, `.`/`D`, `,`/`G`, `S`, `MI`, `PL`,
/// `SG`, `PR`, `FM`, `TH`, `V`, `RN`, `EEEE`, `L` (the C locale's blank currency) and text, quoted
/// (a backslash quoting the next character) or not (a backslash only before a quote).
fn num_to_char(v: &Value, fmt: &str, digits: i64) -> Result<String> {
    #[derive(Clone, Copy, PartialEq)]
    enum K {
        Comma,
        Dec,
        Zero,
        Nine,
        B,
        C,
        D,
        E,
        Fm,
        G,
        L,
        Mi,
        Pl,
        Pr,
        Rn,
        RnLower,
        Sg,
        Sp,
        S,
        Th,
        ThLower,
        V,
    }
    enum Node {
        Key(K),
        Char(String),
    }
    // NUM_keywords, in its order: the first that the text starts with wins
    const KEYS: [(&str, K); 36] = [
        (",", K::Comma),
        (".", K::Dec),
        ("0", K::Zero),
        ("9", K::Nine),
        ("B", K::B),
        ("C", K::C),
        ("D", K::D),
        ("EEEE", K::E),
        ("FM", K::Fm),
        ("G", K::G),
        ("L", K::L),
        ("MI", K::Mi),
        ("PL", K::Pl),
        ("PR", K::Pr),
        ("RN", K::Rn),
        ("SG", K::Sg),
        ("SP", K::Sp),
        ("S", K::S),
        ("TH", K::Th),
        ("V", K::V),
        ("b", K::B),
        ("c", K::C),
        ("d", K::D),
        ("eeee", K::E),
        ("fm", K::Fm),
        ("g", K::G),
        ("l", K::L),
        ("mi", K::Mi),
        ("pl", K::Pl),
        ("pr", K::Pr),
        ("rn", K::RnLower),
        ("sg", K::Sg),
        ("sp", K::Sp),
        ("s", K::S),
        ("th", K::ThLower),
        ("v", K::V),
    ];
    const DECIMAL: u32 = 1 << 1;
    const ZERO: u32 = 1 << 3;
    const BLANK: u32 = 1 << 4;
    const FILLMODE: u32 = 1 << 5;
    const LSIGN: u32 = 1 << 6;
    const BRACKET: u32 = 1 << 7;
    const MINUS: u32 = 1 << 8;
    const PLUS: u32 = 1 << 9;
    const ROMAN: u32 = 1 << 10;
    const MULTI: u32 = 1 << 11;
    const EEEE: u32 = 1 << 14;
    const LSIGN_NONE: i32 = 0;
    const LSIGN_PRE: i32 = 1;
    const LSIGN_POST: i32 = 2;
    let syntax = |m: &str| err::<()>("42601", m.to_string());
    if fmt.is_empty() {
        return Ok(String::new());
    }

    // parse_format and NUMDesc_prepare
    let (mut flag, mut pre, mut post, mut multi, mut lsign, mut pre_lsign_num) =
        (0u32, 0i64, 0i64, 0i64, LSIGN_NONE, 0i64);
    let (mut zero_start, mut zero_end) = (0i64, 0i64);
    let mut nodes: Vec<Node> = vec![];
    let f = fmt.as_bytes();
    let clen = |i: usize| fmt[i..].chars().next().map_or(1, char::len_utf8);
    let mut i = 0;
    while i < f.len() {
        if let Some((name, k)) = KEYS.iter().find(|(name, _)| f[i..].starts_with(name.as_bytes())) {
            i += name.len();
            let k = *k;
            if flag & EEEE != 0 && k != K::E {
                syntax("\"EEEE\" must be the last pattern used")?;
            }
            match k {
                K::Nine => {
                    if flag & BRACKET != 0 {
                        syntax("\"9\" must be ahead of \"PR\"")?;
                    }
                    if flag & MULTI != 0 {
                        multi += 1;
                    } else if flag & DECIMAL != 0 {
                        post += 1;
                    } else {
                        pre += 1;
                    }
                }
                K::Zero => {
                    if flag & BRACKET != 0 {
                        syntax("\"0\" must be ahead of \"PR\"")?;
                    }
                    if flag & ZERO == 0 && flag & DECIMAL == 0 {
                        flag |= ZERO;
                        zero_start = pre + 1;
                    }
                    if flag & DECIMAL == 0 {
                        pre += 1;
                    } else {
                        post += 1;
                    }
                    zero_end = pre + post;
                }
                K::B if pre == 0 && post == 0 && flag & ZERO == 0 => {
                    flag |= BLANK;
                }
                K::D | K::Dec => {
                    if flag & DECIMAL != 0 {
                        syntax("multiple decimal points")?;
                    }
                    if flag & MULTI != 0 {
                        syntax("cannot use \"V\" and decimal point together")?;
                    }
                    flag |= DECIMAL;
                }
                K::Fm => flag |= FILLMODE,
                K::S => {
                    if flag & LSIGN != 0 {
                        syntax("cannot use \"S\" twice")?;
                    }
                    if flag & (PLUS | MINUS | BRACKET) != 0 {
                        syntax("cannot use \"S\" and \"PL\"/\"MI\"/\"SG\"/\"PR\" together")?;
                    }
                    if flag & DECIMAL == 0 {
                        lsign = LSIGN_PRE;
                        pre_lsign_num = pre;
                        flag |= LSIGN;
                    } else if lsign == LSIGN_NONE {
                        lsign = LSIGN_POST;
                        flag |= LSIGN;
                    }
                }
                K::Mi => {
                    if flag & LSIGN != 0 {
                        syntax("cannot use \"S\" and \"MI\" together")?;
                    }
                    flag |= MINUS;
                }
                K::Pl => {
                    if flag & LSIGN != 0 {
                        syntax("cannot use \"S\" and \"PL\" together")?;
                    }
                    flag |= PLUS;
                }
                K::Sg => {
                    if flag & LSIGN != 0 {
                        syntax("cannot use \"S\" and \"SG\" together")?;
                    }
                    flag |= MINUS | PLUS;
                }
                K::Pr => {
                    if flag & (LSIGN | PLUS | MINUS) != 0 {
                        syntax("cannot use \"PR\" and \"S\"/\"PL\"/\"MI\"/\"SG\" together")?;
                    }
                    flag |= BRACKET;
                }
                K::Rn | K::RnLower => flag |= ROMAN,
                K::V => {
                    if flag & DECIMAL != 0 {
                        syntax("cannot use \"V\" and decimal point together")?;
                    }
                    flag |= MULTI;
                }
                K::E => {
                    if flag & EEEE != 0 {
                        syntax("cannot use \"EEEE\" twice")?;
                    }
                    if flag & (BLANK | FILLMODE | LSIGN | BRACKET | MINUS | PLUS | ROMAN | MULTI) != 0 {
                        return Err(super::SqlError {
                            code: "42601",
                            message: "\"EEEE\" is incompatible with other formats".into(),
                            info: Some(Box::new(super::Violation {
                                detail: Some(
                                    "\"EEEE\" may only be used together with digit and decimal point patterns.".into(),
                                ),
                                ..super::Violation::default()
                            })),
                        });
                    }
                    flag |= EEEE;
                }
                _ => {}
            }
            nodes.push(Node::Key(k));
            continue;
        }
        if f[i] == b'"' {
            // a quoted literal: a backslash quotes the next character
            i += 1;
            while i < f.len() {
                if f[i] == b'"' {
                    i += 1;
                    break;
                }
                if f[i] == b'\\' && i + 1 < f.len() {
                    i += 1;
                }
                let n = clen(i);
                nodes.push(Node::Char(fmt[i..i + n].to_string()));
                i += n;
            }
            continue;
        }
        // (outside quotes a backslash is only special before a quote)
        if f[i] == b'\\' && f.get(i + 1) == Some(&b'"') {
            i += 1;
        }
        let n = clen(i);
        nodes.push(Node::Char(fmt[i..i + n].to_string()));
        i += n;
    }
    if flag & EEEE != 0 {
        return num_scientific(v, &fmt.to_ascii_lowercase());
    }

    // the number's digits (numeric_to_char, int8_to_char, float8_to_char)
    let hashes = |pre: i64, post: i64| {
        let mut s = vec![b'#'; (pre + post + 1).max(0) as usize];
        if let Some(c) = s.get_mut(pre.max(0) as usize) {
            *c = b'.';
        }
        s
    };
    let mut out_pre_spaces = 0i64;
    let sign: u8;
    let number: Vec<u8>;
    if flag & ROMAN != 0 {
        let n = match v {
            Value::Float(x) => x.round_ties_even() as i64,
            Value::Int(n) => *n,
            v => int(&super::coerce(v.clone(), super::Type::BigInt)?)?,
        };
        let n = i32::try_from(n).or_else(|_| err(super::RANGE, "integer out of range"))?;
        let roman = num_roman(&Value::Int(i64::from(n)), "FMRN")?;
        number = roman.into_bytes();
        sign = 0;
        pre = 0;
        post = 0;
        lsign = LSIGN_NONE;
        pre_lsign_num = 0;
        flag = (flag & FILLMODE) | ROMAN;
    } else {
        if flag & MULTI != 0 {
            pre += multi;
        }
        let text = match v {
            Value::Float(x) => {
                let x = if flag & MULTI != 0 { x * 10f64.powi(multi as i32) } else { *x };
                // (Postgres's own snprintf writes NaN and Infinity as words)
                let fixed = |p: usize, x: f64| {
                    if x.is_nan() {
                        "NaN".to_string()
                    } else if x.is_infinite() {
                        if x > 0.0 { "Infinity".to_string() } else { "-Infinity".to_string() }
                    } else {
                        format!("{x:.p$}")
                    }
                };
                // (as many digits as the type holds: DBL_DIG, or FLT_DIG for a real)
                let whole = fixed(0, x.abs()).len() as i64;
                if whole >= digits {
                    post = 0;
                } else if whole + post > digits {
                    post = digits - whole;
                }
                fixed(post as usize, x)
            }
            Value::Numeric(d) => {
                let mut d = d.clone();
                if flag & MULTI != 0 && !d.is_special() {
                    let unit = super::decimal::Dec::int(10i64.checked_pow(multi as u32).unwrap_or(i64::MAX));
                    d = super::decimal::Dec::mul(d, unit).or_else(|e| err(super::RANGE, e))?;
                }
                super::round_dec(&d, post as i32)?.to_string()
            }
            v => {
                let mut n = int(v)?;
                if flag & MULTI != 0 {
                    n = 10i64
                        .checked_pow(multi as u32)
                        .and_then(|m| n.checked_mul(m))
                        .map_or_else(|| err(super::RANGE, "bigint out of range"), Ok)?;
                }
                let mut s = n.to_string();
                if post > 0 {
                    s.push('.');
                    s.push_str(&"0".repeat(post as usize));
                }
                s
            }
        };
        let neg = text.starts_with('-');
        let digits = if neg { text.as_bytes()[1..].to_vec() } else { text.into_bytes() };
        sign = if neg { b'-' } else { b'+' };
        let pre_len = digits.iter().position(|&c| c == b'.').unwrap_or(digits.len()) as i64;
        if pre_len < pre {
            out_pre_spaces = pre - pre_len;
            number = digits;
        } else if pre_len > pre {
            number = hashes(pre, post);
        } else {
            number = digits;
        }
    }

    // NUM_processor
    let has = |flag: u32, f: u32| flag & f != 0;
    if zero_start > 0 {
        zero_start -= 1;
    }
    let mut sign_wrote;
    if has(flag, PLUS) || has(flag, MINUS) {
        sign_wrote = !has(flag, PLUS) || has(flag, MINUS);
    } else {
        if sign != b'-' && has(flag, FILLMODE) {
            flag &= !BRACKET;
        }
        sign_wrote = sign == b'+' && has(flag, FILLMODE) && !has(flag, LSIGN);
        if lsign == LSIGN_PRE && pre == pre_lsign_num {
            lsign = LSIGN_POST;
        }
    }
    let mut num_count = post + pre - 1;
    // the last digit after the point worth writing (FM): get_last_relevant_decnum
    let mut last_relevant: Option<usize> = None;
    if has(flag, FILLMODE)
        && has(flag, DECIMAL)
        && let Some(p) = number.iter().position(|&c| c == b'.')
    {
        let mut r = p;
        for (k, &c) in number.iter().enumerate().skip(p + 1) {
            if c != b'0' {
                r = k;
            }
        }
        last_relevant = Some(r);
        if zero_end > out_pre_spaces {
            let last_zero = ((number.len() as i64 - 1).min(zero_end - out_pre_spaces)).max(0) as usize;
            if r < last_zero {
                last_relevant = Some(last_zero);
            }
        }
    }
    if !sign_wrote && out_pre_spaces == 0 {
        num_count += 1;
    }
    let (mut num_in, mut num_curr, mut number_p) = (false, 0i64, 0usize);
    let at = |p: usize| number.get(p).copied().unwrap_or(0);
    let mut out: Vec<u8> = vec![];
    let fm = has(flag, FILLMODE);
    for node in &nodes {
        let k = match node {
            Node::Char(s) => {
                out.extend_from_slice(s.as_bytes());
                continue;
            }
            Node::Key(k) => *k,
        };
        match k {
            K::Nine | K::Zero | K::Dec | K::D => {
                // NUM_numpart_to_char
                if has(flag, ROMAN) {
                    continue;
                }
                num_in = false;
                let predec_space = !has(flag, ZERO) && number_p == 0 && at(0) == b'0' && post != 0;
                let relevant_point = last_relevant.is_some_and(|r| at(r) == b'.');
                if !sign_wrote
                    && (num_curr >= out_pre_spaces || (has(flag, ZERO) && zero_start == num_curr))
                    && (!predec_space || relevant_point)
                {
                    if has(flag, LSIGN) {
                        if lsign == LSIGN_PRE {
                            out.push(if sign == b'-' { b'-' } else { b'+' });
                            sign_wrote = true;
                        }
                    } else if has(flag, BRACKET) {
                        out.push(if sign == b'+' { b' ' } else { b'<' });
                        sign_wrote = true;
                    } else if sign == b'+' {
                        if !fm {
                            out.push(b' ');
                        }
                        sign_wrote = true;
                    } else if sign == b'-' {
                        out.push(b'-');
                        sign_wrote = true;
                    }
                }
                if num_curr < out_pre_spaces && (zero_start > num_curr || !has(flag, ZERO)) {
                    if !fm {
                        out.push(b' ');
                    }
                } else if has(flag, ZERO) && num_curr < out_pre_spaces && zero_start <= num_curr {
                    out.push(b'0');
                    num_in = true;
                } else {
                    if at(number_p) == b'.' {
                        if !relevant_point || fm {
                            out.push(b'.');
                        }
                    } else if last_relevant.is_some_and(|r| number_p > r) && k != K::Zero {
                        // (past the last digit worth writing)
                    } else if predec_space {
                        if !fm {
                            out.push(b' ');
                        } else if relevant_point {
                            out.push(b'0');
                        }
                    } else {
                        out.push(at(number_p)); // (past the end: a NUL, which ends the text)
                        num_in = true;
                    }
                    if number_p < number.len() {
                        number_p += 1;
                    }
                }
                let mut end = num_count + i64::from(out_pre_spaces != 0) + i64::from(has(flag, DECIMAL));
                if last_relevant == Some(number_p) {
                    end = num_curr;
                }
                if num_curr + 1 == end {
                    if sign_wrote && has(flag, BRACKET) {
                        out.push(if sign == b'+' { b' ' } else { b'>' });
                    } else if has(flag, LSIGN) && lsign == LSIGN_POST {
                        out.push(if sign == b'-' { b'-' } else { b'+' });
                    }
                }
                num_curr += 1;
            }
            K::Comma | K::G => {
                if num_in {
                    out.push(b',');
                } else if !fm {
                    out.push(b' ');
                }
            }
            K::L => out.push(b' '),
            K::Rn | K::RnLower => {
                let rest = String::from_utf8_lossy(&number[number_p.min(number.len())..]).to_string();
                let rest = if k == K::RnLower { rest.to_ascii_lowercase() } else { rest };
                out.extend_from_slice(if fm { rest } else { format!("{rest:>15}") }.as_bytes());
            }
            K::Th | K::ThLower => {
                if has(flag, ROMAN) || number.first() == Some(&b'#') || sign == b'-' || has(flag, DECIMAL) {
                    continue;
                }
                let last = number.last().copied().unwrap_or(0);
                if !last.is_ascii_digit() {
                    let shown = String::from_utf8_lossy(&number).to_string();
                    return err("22P02", format!("\"{shown}\" is not a number"));
                }
                let teen = number.len() > 1 && number[number.len() - 2] == b'1';
                let th = match (last, teen) {
                    (_, true) => "th",
                    (b'1', _) => "st",
                    (b'2', _) => "nd",
                    (b'3', _) => "rd",
                    _ => "th",
                };
                out.extend_from_slice(if k == K::Th { th.to_ascii_uppercase() } else { th.to_string() }.as_bytes());
            }
            K::Mi => {
                if sign == b'-' {
                    out.push(b'-');
                } else if !fm {
                    out.push(b' ');
                }
            }
            K::Pl => {
                if sign == b'+' {
                    out.push(b'+');
                } else if !fm {
                    out.push(b' ');
                }
            }
            K::Sg => out.push(sign),
            _ => {}
        }
    }
    // (a NUL written ends the text, as in C)
    if let Some(nul) = out.iter().position(|&c| c == 0) {
        out.truncate(nul);
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// A SIMILAR TO pattern as a regular expression, as Postgres's `similar_to_escape` makes it:
/// anchored, `%` and `_` as `.*` and `.`, groups non-capturing, `escape` quoting the next character.
/// A LIKE pattern with `escape` (none: no escape character) as one with a backslash, which is what
/// `like` reads: the escape character makes the next one literal, and a backslash is only itself.
fn like_escape(p: &str, escape: Option<char>) -> String {
    let mut r = String::with_capacity(p.len() + 2);
    let mut after_escape = false;
    for c in p.chars() {
        if after_escape {
            if c == '\\' {
                r.push('\\');
            }
            r.push(c);
            after_escape = false;
        } else if Some(c) == escape {
            r.push('\\');
            after_escape = true;
        } else if c == '\\' {
            r.push_str("\\\\");
        } else {
            r.push(c);
        }
    }
    r
}

/// An ESCAPE of LIKE, SIMILAR TO or substring(... SIMILAR ...) longer than a character, as Postgres refuses it.
fn bad_escape() -> super::SqlError {
    super::SqlError {
        code: "22025",
        message: "invalid escape string".into(),
        info: Some(Box::new(super::Violation {
            hint: Some("Escape string must be empty or one character.".into()),
            ..super::Violation::default()
        })),
    }
}

fn similar_escape(p: &str, escape: Option<char>) -> Result<String> {
    fits(p.len().checked_mul(3).and_then(|n| n.checked_add(16)))?; // as Postgres sizes it: `(` grows to `(?:`
    let mut r = String::from("^(?:");
    let (mut after_escape, mut in_class, mut quotes) = (false, false, 0);
    for c in p.chars() {
        if after_escape {
            if c == '"' && !in_class {
                r.push_str(match quotes {
                    0 => "){1,1}?(",
                    1 => "){1,1}(?:",
                    _ => {
                        return err(
                            "2200C",
                            "SQL regular expression may not contain more than two escape-double-quote separators",
                        );
                    }
                });
                quotes += 1;
            } else {
                r.push('\\');
                r.push(c);
            }
            after_escape = false;
        } else if Some(c) == escape {
            after_escape = true;
        } else if in_class {
            if c == '\\' {
                r.push('\\');
            }
            r.push(c);
            in_class = c != ']';
        } else {
            match c {
                '[' => {
                    r.push(c);
                    in_class = true;
                }
                '%' => r.push_str(".*"),
                '_' => r.push('.'),
                '(' => r.push_str("(?:"),
                '\\' | '.' | '^' | '$' => {
                    r.push('\\');
                    r.push(c);
                }
                _ => r.push(c),
            }
        }
    }
    if after_escape {
        return err("22025", "invalid escape string");
    }
    r.push_str(")$");
    Ok(r)
}

/// `regexp_count`, `regexp_substr`, `regexp_instr` and `regexp_replace` with their start, N,
/// flags, end option and sub-expression arguments.
/// A regexp function's SQL name, as its errors give it.
fn regexp_name(f: Scalar) -> &'static str {
    match f {
        Scalar::RegexpCount => "regexp_count",
        Scalar::RegexpSubstr => "regexp_substr",
        Scalar::RegexpInstr => "regexp_instr",
        Scalar::RegexpLike => "regexp_like",
        Scalar::RegexpSplit => "regexp_split_to_array",
        Scalar::RegexpMatch => "regexp_match",
        _ => "regexp_replace",
    }
}

fn regexp_ext(f: Scalar, a: &[Value]) -> Result<Value> {
    use Scalar::*;
    use Value::{Int, Null, Text};
    let arg = |i: usize, name: &str, min: i64| -> Result<Option<i64>> {
        match a.get(i) {
            Some(Value::Int(n)) if *n < min => err("22023", format!("invalid value for parameter \"{name}\": {n}")),
            Some(v) => Ok(Some(int(v)?)),
            None => Ok(None),
        }
    };
    // the position of each argument in each function
    let (start_at, n_at, end_at, flags_at, sub_at) = match f {
        RegexpCount => (2, 9, 9, 3, 9),
        RegexpSubstr => (2, 3, 9, 4, 5),
        RegexpInstr => (2, 3, 4, 5, 6),
        _ => (3, 4, 9, 5, 9), // regexp_replace(s, p, r, start, N, flags)
    };
    let (flags, global) = super::regex_flags(a.get(flags_at))?;
    if global && f != RegexpReplace {
        return err("22023", format!("{}() does not support the \"global\" option", regexp_name(f)));
    }
    let re = super::compiled(&text(&a[1]), &flags)?;
    let start = arg(start_at, "start", 1)?.unwrap_or(1);
    let n = arg(n_at, "n", if f == RegexpReplace { 0 } else { 1 })?.unwrap_or(1);
    let endopt = arg(end_at, "endoption", 0)?.unwrap_or(0);
    if endopt > 1 {
        return err("22023", format!("invalid value for parameter \"endoption\": {endopt}"));
    }
    let sub = arg(sub_at, "subexpr", 0)?.unwrap_or(0) as usize;
    let cs = chars(&text(&a[0]))?;
    let from = (start as usize - 1).min(cs.len() + 1);
    if f == RegexpReplace {
        let tail: String = cs.get(from..).unwrap_or_default().iter().collect();
        let head: String = cs[..from.min(cs.len())].iter().collect();
        // (an N given wins over the g flag, as textregexreplace_extended has it)
        let all = n == 0 || (global && a.len() <= n_at);
        if all {
            return Ok(Text(head + &super::regex::replace(&re, &tail, &text(&a[2]), true)?));
        }
        // the Nth match only
        let tcs = chars(&tail)?;
        let (mut at, mut k) = (0, 0);
        while at <= tcs.len() {
            let Some(caps) = re.find_at(&tcs, at)? else { break };
            let (ms, me) = caps[0].unwrap();
            k += 1;
            if k == n {
                let mut out = head + &tcs[..ms].iter().collect::<String>();
                super::regex::expand(&mut out, &tcs, &caps, &text(&a[2]))?;
                out.extend(&tcs[me..]);
                return Ok(Text(out));
            }
            at = if me == ms { me + 1 } else { me };
        }
        return Ok(Text(text(&a[0])));
    }
    let mut at = from;
    let (mut k, mut found) = (0, None);
    let mut count = 0i64;
    while at <= cs.len() {
        let Some(caps) = re.find_at(&cs, at)? else { break };
        let (ms, me) = caps[0].unwrap();
        count += 1;
        k += 1;
        if k == n && found.is_none() {
            found = Some(caps.clone());
            if f != RegexpCount {
                break;
            }
        }
        if ms >= cs.len() {
            break;
        }
        at = if me == ms { me + 1 } else { me };
    }
    Ok(match f {
        RegexpCount => Int(count),
        _ => {
            let Some(caps) = found else { return Ok(if f == RegexpInstr { Int(0) } else { Null }) };
            match caps.get(sub).copied().flatten() {
                None => {
                    if f == RegexpInstr {
                        Int(0)
                    } else {
                        Null
                    }
                }
                Some((x, y)) if f == RegexpInstr => Int(if endopt == 1 { y } else { x } as i64 + 1),
                Some((x, y)) => Text(cs[x..y].iter().collect()),
            }
        }
    })
}

/// `jsonb_set` for `jsonb_set_lax`, with `value` (not its third argument, which may be null).
fn json_set_lax_value(a: &[Value], value: Value) -> Result<Value> {
    let Some(steps) = path_steps(&a[1])? else { return Ok(Value::Null) };
    let create = match a.get(3) {
        Some(c) => super::truth(c)?.unwrap_or(true),
        None => true,
    };
    Ok(Value::Json(super::jsonb::shallow(super::jsonb::set(
        &super::as_json(a[0].clone())?,
        &steps,
        &super::as_json(value)?,
        create,
    )?)?))
}

/// A path argument's steps (a text[]), None where an element is null; None for a null path.
fn path_steps(v: &Value) -> Result<Option<Vec<Option<String>>>> {
    Ok(super::array_values(v.clone())?.map(|xs| {
        xs.iter()
            .map(|x| match x {
                Value::Null => None,
                x => Some(x.to_text()),
            })
            .collect()
    }))
}

/// The values Postgres's degree functions are normalized by, worked out once from the radian
/// functions so that `sind(30)` is exactly 0.5 and `tand(45)` exactly 1.
struct Degrees {
    sin_30: f64,
    one_minus_cos_60: f64,
    asin_0_5: f64,
    acos_0_5: f64,
    atan_1_0: f64,
    tan_45: f64,
    cot_45: f64,
}

fn degree_constants() -> &'static Degrees {
    static C: std::sync::OnceLock<Degrees> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        let mut c = Degrees {
            sin_30: (30.0 * RADIANS_PER_DEGREE).sin(),
            one_minus_cos_60: 1.0 - (60.0 * RADIANS_PER_DEGREE).cos(),
            asin_0_5: 0.5f64.asin(),
            acos_0_5: 0.5f64.acos(),
            atan_1_0: 1.0f64.atan(),
            tan_45: 1.0,
            cot_45: 1.0,
        };
        c.tan_45 = sind_q1(&c, 45.0) / cosd_q1(&c, 45.0);
        c.cot_45 = cosd_q1(&c, 45.0) / sind_q1(&c, 45.0);
        c
    })
}

fn sind_0_to_30(c: &Degrees, x: f64) -> f64 {
    ((x * RADIANS_PER_DEGREE).sin() / c.sin_30) / 2.0
}

fn cosd_0_to_60(c: &Degrees, x: f64) -> f64 {
    1.0 - (((1.0 - (x * RADIANS_PER_DEGREE).cos()) / c.one_minus_cos_60) / 2.0)
}

fn sind_q1(c: &Degrees, x: f64) -> f64 {
    if x <= 30.0 { sind_0_to_30(c, x) } else { cosd_0_to_60(c, 90.0 - x) }
}

fn cosd_q1(c: &Degrees, x: f64) -> f64 {
    if x <= 60.0 { cosd_0_to_60(c, x) } else { sind_0_to_30(c, 90.0 - x) }
}

fn asind_q1(c: &Degrees, x: f64) -> f64 {
    if x <= 0.5 { (x.asin() / c.asin_0_5) * 30.0 } else { 90.0 - (x.acos() / c.acos_0_5) * 60.0 }
}

fn acosd_q1(c: &Degrees, x: f64) -> f64 {
    if x <= 0.5 { 90.0 - (x.asin() / c.asin_0_5) * 30.0 } else { (x.acos() / c.acos_0_5) * 60.0 }
}

/// `sind`, `cosd`, `tand`, `cotd`, `asind`, `acosd`, `atand`: Postgres's versions, which reduce
/// the argument to the first quadrant and scale by a known value, so the exact cases come out exact.
fn degree_fn(f: Scalar, x: f64) -> Result<f64> {
    use Scalar::*;
    let c = degree_constants();
    let range = || err(super::RANGE, "input is out of range");
    if x.is_nan() {
        return Ok(f64::NAN);
    }
    Ok(match f {
        Sind | Cosd | Tand | Cotd => {
            if x.is_infinite() {
                return range();
            }
            let mut sign = 1.0;
            let mut a = x % 360.0;
            match f {
                Cosd => {
                    a = a.abs();
                    if a > 180.0 {
                        a = 360.0 - a;
                    }
                    if a > 90.0 {
                        a = 180.0 - a;
                        sign = -1.0;
                    }
                    sign * cosd_q1(c, a)
                }
                _ => {
                    if a < 0.0 {
                        a = -a;
                        sign = -1.0;
                    }
                    if a > 180.0 {
                        a = 360.0 - a;
                        sign = -sign;
                    }
                    match f {
                        Sind => {
                            if a > 90.0 {
                                a = 180.0 - a;
                            }
                            sign * sind_q1(c, a)
                        }
                        Tand => {
                            if a > 90.0 {
                                a = 180.0 - a;
                                sign = -sign;
                            }
                            let t = sind_q1(c, a) / cosd_q1(c, a);
                            sign * (t / c.tan_45)
                        }
                        _ => {
                            if a > 90.0 {
                                a = 180.0 - a;
                                sign = -sign;
                            }
                            sign * ((cosd_q1(c, a) / sind_q1(c, a)) / c.cot_45)
                        }
                    }
                }
            }
        }
        Asind | Acosd => {
            if !(-1.0..=1.0).contains(&x) {
                return range();
            }
            if f == Asind {
                if x < 0.0 { -asind_q1(c, -x) } else { asind_q1(c, x) }
            } else if x < 0.0 {
                90.0 + asind_q1(c, -x)
            } else {
                acosd_q1(c, x)
            }
        }
        _ => (x.atan() / c.atan_1_0) * 45.0,
    })
}

/// `a[i] = v`, `a[lo:hi] = v` (in UPDATE ... SET): the array with its element or slice replaced; a
/// place past either end grows the array with nulls, and a place before its start moves its lower bound.
fn array_set(a: &[Value]) -> Result<Value> {
    use Value::Null;
    // (a jsonb's key or element: the binder marks it with a jsonb fourth argument)
    if matches!(a.get(3), Some(Value::Json(_))) {
        return json_set(a);
    }
    let v = &a[0];
    if a[1] == Null || a.get(3) == Some(&Null) {
        return err("22004", "array subscript in assignment must not be null");
    }
    if a.get(3).is_some() && a[2] == Null {
        return Ok(v.clone()); // a null source leaves a slice as it was
    }
    // (a null array is an empty one; see `mdarray::Arr::set` and `set_slice`)
    let x = super::mdarray::read(v)?.unwrap_or_else(super::mdarray::Arr::empty);
    let out = match a.get(3) {
        Some(h) => {
            let src = super::mdarray::read(&a[2])?.map(|s| s.elems).unwrap_or_default();
            x.set_slice(&[(int(&a[1])?, int(h)?)], &src)?
        }
        None => x.set(&[int(&a[1])?], a[2].clone())?,
    };
    Ok(out.value())
}

/// a[i][j]... = v and a[lo:hi][lo:hi]... = v (arguments: the array, the value, whether a slice, the subscripts, two
/// for each range): the array with that element or slice set, as Postgres's array_set_element and array_set_slice
/// do (see `mdarray`); a null source leaves a slice as it was, a null subscript is 22004.
fn array_set_n(a: &[Value]) -> Result<Value> {
    use Value::Null;
    let (v, value, slice) = (&a[0], &a[1], a[2] == Value::Bool(true));
    let subs = &a[3..];
    if subs.contains(&Null) {
        return err("22004", "array subscript in assignment must not be null");
    }
    if slice && *value == Null {
        return Ok(v.clone());
    }
    let x = super::mdarray::read(v)?.unwrap_or_else(super::mdarray::Arr::empty);
    let out = if slice {
        let mut ranges = vec![];
        for p in subs.chunks(2) {
            ranges.push((int(&p[0])?, int(p.get(1).unwrap_or(&p[0]))?));
        }
        let src = super::mdarray::read(value)?.map(|s| s.elems).unwrap_or_default();
        x.set_slice(&ranges, &src)?
    } else {
        let at = subs.iter().map(int).collect::<Result<Vec<_>>>()?;
        x.set(&at, value.clone())?
    };
    Ok(out.value())
}

/// `x[k] = v` of a jsonb `x` (arguments x, k, v), as Postgres's jsonb subscripting assigns one step of its path: a
/// key of an object (a number is its text), or an element of an array, counted from 0 (below 0, from the end), with
/// nulls filling the gap past the end; a null `x` is the empty object, or array for a number `k`, the key implies.
/// A chain (`x['a'][0] = v`) sets each step's part of the one before (see `assigned_at`), so a missing part is
/// made; a scalar has no part to set (22023).
fn json_set(a: &[Value]) -> Result<Value> {
    use serde_json::Value as J;
    if a[1] == Value::Null {
        return err("22004", "jsonb subscript in assignment must not be null");
    }
    let new = match &a[2] {
        Value::Null => J::Null,
        Value::Json(j) => j.clone(),
        v => super::as_json(v.clone())?,
    };
    let mut base = match &a[0] {
        Value::Null if matches!(a[1], Value::Int(_)) => J::Array(vec![]),
        Value::Null => J::Object(Default::default()),
        Value::Json(j) => j.clone(),
        v => super::as_json(v.clone())?,
    };
    match &mut base {
        J::Object(o) => {
            o.insert(text(&a[1]), new);
        }
        J::Array(xs) => {
            let k = text(&a[1]);
            let Ok(i) = k.trim().parse::<i64>() else {
                return err("22P02", format!("path element at position 1 is not an integer: \"{k}\""));
            };
            let at = if i < 0 { i + xs.len() as i64 } else { i };
            let Ok(at) = usize::try_from(at) else {
                return err("22023", format!("path element at position 1 is out of range: {i}"));
            };
            if at >= xs.len() {
                fits(at.checked_add(1).and_then(|n| n.checked_mul(ITEM)))?;
                xs.resize(at + 1, J::Null);
            }
            xs[at] = new;
        }
        _ => {
            return Err(super::SqlError {
                code: "22023",
                message: "cannot replace existing key".into(),
                info: Some(Box::new(super::Violation {
                    detail: Some("The path assumes key is a composite object, but it is a scalar value.".into()),
                    ..Default::default()
                })),
            });
        }
    }
    Ok(Value::Json(base))
}

/// Which of the `jsonb_path_*` functions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PathFn {
    Exists,
    Match,
    QueryArray,
    QueryFirst,
}

/// The arguments of the `jsonb_path_*` functions: the path parsed, the target, the variables
/// (`{}` if left out: they must be an object) and whether errors are silent; None if an argument
/// is null (the functions are strict).
pub(super) fn path_args(a: &[Value]) -> Result<Option<(jsonpath::Path, jsonpath::Value, jsonpath::Value, bool)>> {
    if a.iter().take(4).any(|v| *v == Value::Null) {
        return Ok(None);
    }
    let path = jsonpath::parse_code(&a[1].to_text()).map_err(super::path_error)?;
    let target = jsonpath::Value::from_json(&super::as_json(a[0].clone())?);
    let vars = match a.get(2) {
        Some(v) => super::as_json(v.clone())?,
        None => serde_json::Value::Object(Default::default()),
    };
    if !vars.is_object() {
        return Err(super::SqlError {
            code: "22023",
            message: "\"vars\" argument is not an object".into(),
            info: Some(Box::new(super::Violation {
                detail: Some("Jsonpath parameters should be encoded as key-value pairs of \"vars\" object.".into()),
                ..Default::default()
            })),
        });
    }
    let silent = match a.get(3) {
        Some(v) => super::truth(v)?.unwrap_or(false),
        None => false,
    };
    Ok(Some((path, target, jsonpath::Value::from_json(&vars), silent)))
}

/// Whether `s` names a text search configuration.
fn is_config(s: &str) -> bool {
    super::tsearch::config(s).is_ok()
}

/// The XML functions (see `xmlfn`). XmlElement and XmlForest end with two masks: bit i set where argument i is
/// already XML (its text goes in as it is; anything else is mapped and escaped), and where it is an array.
fn xml_call(f: Scalar, a: &[Value]) -> Result<Value> {
    use super::xmlfn as x;
    use Scalar::*;
    use Value::{Bool, Null, Text};
    let fail = |code: &'static str, message: &str, detail: String| super::SqlError {
        code,
        message: message.into(),
        info: Some(Box::new(super::Violation { detail: (!detail.is_empty()).then_some(detail), ..Default::default() })),
    };
    // (XML OPTION DOCUMENT: xml values are read as documents)
    let documents = || super::roles::setting_now("xmloption").is_some_and(|o| o.eq_ignore_ascii_case("document"));
    // an argument of type xml given as text: read as the cast to xml reads it
    let as_xml = |v: &Value| -> Result<String> {
        let t = v.to_text();
        super::coerce(Text(t.clone()), Type::Xml)?;
        Ok(t)
    };
    // an XPath call's error as Postgres reports it (`two`: xpath or xpath_exists of two arguments, a SQL function
    // calling the one of three, which the error's CONTEXT names)
    let xerr = |e: x::XErr, two: Option<&str>| -> super::SqlError {
        let mut er = match e {
            x::XErr::Doc(d) => fail("2200M", "could not parse XML document", d),
            x::XErr::Empty => fail("22000", "empty XPath expression", String::new()),
            x::XErr::Path(d) => fail("XX000", "invalid XPath expression", d),
            x::XErr::Eval(d) => fail("XX000", "could not create XPath object", d),
            x::XErr::Many => fail("21000", "more than one value returned by column XPath expression", String::new()),
        };
        if let (Some(name), Some(info)) = (two, er.info.as_mut()) {
            info.context = Some(format!("SQL function \"{name}\" statement 1"));
        }
        er
    };
    // xpath's third argument: a two-dimensional array of prefixes and URIs
    let namespaces = |v: Option<&Value>| -> Result<Vec<(String, String)>> {
        let Some(v) = v.filter(|v| **v != Null) else { return Ok(vec![]) };
        let items = super::array_items(v.clone())?.unwrap_or_default();
        let mut out = vec![];
        for pair in items.chunks(2) {
            match pair {
                [p, u] if *p != Null && *u != Null => out.push((p.to_text(), u.to_text())),
                [_, _] => return err("22004", "neither namespace name nor URI may be null"),
                _ => return err("22000", "invalid array for XML namespace mapping"),
            }
        }
        Ok(out)
    };
    let warn = |w: String| {
        if !w.is_empty() {
            super::notice("WARNING", w);
        }
    };
    Ok(match f {
        XmlComment => {
            if a[0] == Null {
                return Ok(Null);
            }
            let t = a[0].to_text();
            if t.contains("--") || t.ends_with('-') {
                return Err(fail("2200S", "invalid XML comment", String::new()));
            }
            Text(format!("<!--{t}-->"))
        }
        // xmlconcat: the values one after another, their XML declarations made one (a version where all give the
        // same; standalone yes where all say yes, no where any says no and none says nothing)
        XmlConcat => {
            let parts: Vec<String> = a.iter().filter(|v| **v != Null).map(|v| v.to_text()).collect();
            if parts.is_empty() {
                return Ok(Null);
            }
            let (mut standalone, mut version, mut no_version) = (1i8, None::<String>, false);
            let mut body = String::new();
            for p in &parts {
                let d = x::decl(p).unwrap_or_default();
                if d.standalone == 0 && standalone == 1 {
                    standalone = 0;
                }
                if d.standalone < 0 {
                    standalone = -1;
                }
                match (&d.version, &version) {
                    (None, _) => no_version = true,
                    (Some(v), None) => version = Some(v.clone()),
                    (Some(v), Some(g)) if v != g => no_version = true,
                    _ => {}
                }
                body.push_str(&p[d.len..]);
            }
            if !no_version || standalone >= 0 {
                body = x::decl_text(if no_version { None } else { version.as_deref() }, standalone) + &body;
            }
            Text(body)
        }
        XmlParse => {
            if a[0] == Null {
                return Ok(Null);
            }
            let (t, doc) = (a[0].to_text(), a[1].to_text() == "document");
            match x::read(&t, doc, false) {
                Ok(_) => {}
                Err(x::Invalid::Decl(d)) => {
                    return Err(fail("2200N", "invalid XML content: invalid XML declaration", d.into()));
                }
                Err(x::Invalid::Errors(e)) if doc => return Err(fail("2200M", "invalid XML document", e)),
                Err(x::Invalid::Errors(e)) => return Err(fail("2200N", "invalid XML content", e)),
            }
            Text(t)
        }
        // XMLSERIALIZE: the text as it is, unless DOCUMENT (which it must be) or INDENT (formatted) was asked for
        XmlSerialize => {
            if a[0] == Null {
                return Ok(Null);
            }
            let t = a[0].to_text();
            let doc = a[1].to_text() == "document";
            let indent = a.get(2).is_some_and(|v| v.to_text() == "indent");
            let not_doc = || fail("2200L", "not an XML document", String::new());
            if indent {
                return Ok(Text(x::indent(&t, doc).ok_or_else(not_doc)?));
            }
            if doc && x::read(&t, true, false).is_err() {
                return Err(not_doc());
            }
            Text(t)
        }
        IsDocument => {
            if a[0] == Null {
                return Ok(Null);
            }
            Bool(x::read(&as_xml(&a[0])?, true, false).is_ok())
        }
        XmlWellFormed(k) => {
            if a[0] == Null {
                return Ok(Null);
            }
            let doc = match k {
                1 => false,
                2 => true,
                _ => documents(),
            };
            Bool(x::read(&a[0].to_text(), doc, false).is_ok())
        }
        XmlPi => {
            let name = a[0].to_text();
            let pi = |d: &str| fail("42601", "invalid XML processing instruction", d.into());
            if name.eq_ignore_ascii_case("xml") {
                return Err(pi("XML processing instruction target name cannot be \"xml\"."));
            }
            let name = x::ident_name(&name);
            match a.get(1) {
                None => Text(format!("<?{name}?>")),
                Some(Null) => Null,
                Some(c) => {
                    let c = c.to_text();
                    if c.contains("?>") {
                        return Err(pi("XML processing instruction cannot contain \"?>\"."));
                    }
                    // (its leading spaces dropped)
                    Text(format!("<?{name} {}?>", c.trim_start_matches(' ')))
                }
            }
        }
        // XMLROOT: the value's XML declaration made again with the version given (none: NO VALUE) and the standalone
        // given (its own where none is)
        XmlRoot => {
            if a[0] == Null {
                return Ok(Null);
            }
            let body = a[0].to_text();
            let d = x::decl(&body).unwrap_or_default();
            let version = if a[1] == Null { None } else { Some(a[1].to_text()) };
            let standalone = match a[2].to_text().as_str() {
                "yes" => 1,
                "no" => 0,
                "omitted" => d.standalone,
                _ => -1,
            };
            Text(x::decl_text(version.as_deref(), standalone) + &body[d.len..])
        }
        XmlExistsFn | XpathExists => {
            if a[0] == Null || a[1] == Null {
                return Ok(Null);
            }
            let two = (f == XpathExists && a.len() == 2).then_some("xpath_exists");
            let ns = namespaces(a.get(2))?;
            let (any, w) = x::xpath_exists(&a[0].to_text(), &as_xml(&a[1])?, &ns).map_err(|e| xerr(e, two))?;
            warn(w);
            Bool(any)
        }
        Xpath => {
            if a[0] == Null || a[1] == Null {
                return Ok(Null);
            }
            let two = (a.len() == 2).then_some("xpath");
            let ns = namespaces(a.get(2))?;
            let (found, w) = x::xpath(&a[0].to_text(), &as_xml(&a[1])?, &ns).map_err(|e| xerr(e, two))?;
            warn(w);
            Text(super::array_text(found.into_iter().map(Text).collect::<Vec<_>>().iter()))
        }
        XmlElement | XmlForest => {
            let (mask, arrays, args) = match a {
                [rest @ .., Value::Int(m), Value::Int(r)] => (*m, *r, rest),
                _ => (0, 0, a),
            };
            let shown = |i: usize, v: &Value| -> Result<String> {
                if mask >> i & 1 == 1 {
                    return Ok(v.to_text());
                }
                // (an array: each element, but a null, as an <element>)
                if arrays >> i & 1 == 1 {
                    let mut out = String::new();
                    for e in super::array_items(v.clone())?.unwrap_or_default() {
                        if e != Null {
                            out += &format!("<element>{}</element>", xml_value(&e)?);
                        }
                    }
                    return Ok(out);
                }
                xml_value(v)
            };
            if f == XmlForest {
                let mut out = String::new();
                for (k, pair) in args.chunks(2).enumerate() {
                    if let [n, v] = pair {
                        if *v == Null {
                            continue;
                        }
                        let name = x::ident_name(&n.to_text());
                        out += &format!("<{name}>{}</{name}>", shown(2 * k + 1, v)?);
                    }
                }
                return Ok(if out.is_empty() { Null } else { Text(out) });
            }
            let name = x::ident_name(&args[0].to_text());
            let nattr = match args.get(1) {
                Some(Value::Int(n)) => *n as usize,
                _ => 0,
            };
            let mut attrs = String::new();
            for k in 0..nattr {
                let (an, av) = (&args[2 + 2 * k], &args[3 + 2 * k]);
                if *av == Null {
                    continue;
                }
                // (an attribute's value is escaped, XML or not)
                let v = if mask >> (3 + 2 * k) & 1 == 1 { x::escape_text(&av.to_text()) } else { xml_value(av)? };
                attrs += &format!(" {}=\"{}\"", x::ident_name(&an.to_text()), v.replace('"', "&quot;"));
            }
            let first = 2 + 2 * nattr;
            let mut body = String::new();
            for (i, v) in args.iter().enumerate().skip(first) {
                if *v != Null {
                    body += &shown(i, v)?;
                }
            }
            Text(if body.is_empty() { format!("<{name}{attrs}/>") } else { format!("<{name}{attrs}>{body}</{name}>") })
        }
        _ => unreachable!("not an XML function"),
    })
}

/// A value as XML content (map_sql_value_to_xml_value, escaped): booleans as true and false, dates and times in
/// ISO 8601 (an infinite one is out of range: XML has none), binary strings as xmlbinary says (base64 or hex).
fn xml_value(v: &Value) -> Result<String> {
    let infinite = |what: &str, kind: &str| super::SqlError {
        code: "22008",
        message: format!("{what} out of range"),
        info: Some(Box::new(super::Violation {
            detail: Some(format!("XML does not support infinite {kind} values.")),
            ..Default::default()
        })),
    };
    Ok(match v {
        Value::Date(d) if *d == i32::MAX || *d == i32::MIN => return Err(infinite("date", "date")),
        Value::Timestamp(t) | Value::TimestampTz(t) if *t == i64::MAX || *t == i64::MIN => {
            return Err(infinite("timestamp", "timestamp"));
        }
        Value::Bool(b) => b.to_string(),
        Value::Bytea(b) => {
            if super::roles::setting_now("xmlbinary").is_some_and(|s| s.eq_ignore_ascii_case("hex")) {
                b.iter().map(|c| format!("{c:02X}")).collect()
            } else {
                super::xmlmap::base64(b)
            }
        }
        v => super::xmlfn::escape_text(&super::xmlfn::value_text(v)),
    })
}

/// The C library's error functions, which Postgres's erf and erfc call (Rust's own are not stable yet).
mod cmath {
    unsafe extern "C" {
        pub safe fn erf(x: f64) -> f64;
        pub safe fn erfc(x: f64) -> f64;
    }
}
