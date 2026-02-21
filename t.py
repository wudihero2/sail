import pyspark.sql.functions as sf
from pyspark.sql import SparkSession

spark = SparkSession.builder.remote("sc://localhost:50051").getOrCreate()
actual = spark.sql("SELECT format_string('Hello World %d %s', 100, 'days') AS result").toPandas()
# df = spark.createDataFrame([(5, "hello")], ["a", "b"])
# df.select("*", sf.format_string('%d %s', "a", df.b)).show()
print(actual)
spark.stop()